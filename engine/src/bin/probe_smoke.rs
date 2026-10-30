//! PROBE SMOKE — testnet-e2e workflow'unun gerçek order adımı.
//!
//! KULLANIM (YALNIZ testnet, ELLE):
//!   TESTNET=1 probe_smoke <COIN> [notional=20]
//!
//! Akış:
//!   1. mainnet guard: TESTNET=1 yoksa HİÇBİR şey yapmaz (HATA).
//!   2. küçük limit order yerleştirir (mid ×0.70 — derin bid, dinlenir).
//!      notional varsayılanı $20: etkin bant değeri 20×0.70=$14 → borsa
//!      minimumu $10'un ÜSTÜNDE ($12 seçildiğinde 0.70 bantta etkin
//!      $8.4 kalır ve emir borsaya hiç gitmez).
//!   3. emri KENDİ OID'siyle açık-listede DOĞRULAR (coin adı yetmez).
//!   4. İPTAL eder; iptali OID bazında tekrar sorguyla DOĞRULAR.
//!   5. pozisyon FLAT doğrular — REST hatası FAIL sayılır (asla flat
//!      varsayılmaz).
//!
//!   Her adımda REST hatası → probe BAŞARISIZ (exit ≠ 0).
//!
//! Bu binary normal bot koşusunun PARÇASI DEĞİLDİR — yalnız elle smoke.

use anyhow::{Context, Result};
use hyperliquid_api::{Config, Info, Tif};

/// FLAT doğrulama — REST hatası → Err (başarı SAYILMAZ).
/// Coin satırı hiç yoksa flat kabul edilir (borsa rapor etmiyor).
async fn assert_flat(ex: &mut engine::Executor, coin: &str) -> Result<()> {
    ex.refresh_positions().await.with_context(|| {
        format!("smoke: {coin} pozisyon sorgusu başarısız — flat DOĞRULANAMADI")
    })?;
    let p = ex.positions().get(coin);
    let flat = p.map(|x| x.szi.abs() < 1e-12).unwrap_or(true);
    if !flat {
        anyhow::bail!(
            "smoke: pozisyon KAPALI DEĞİL (szi={:?}) — manuel temizlik gerekli",
            p.map(|x| x.szi)
        );
    }
    Ok(())
}

/// Emrin açık listeye düşmesini bekle (eventual consistency: place sonrası
/// ilk sorgu listeyi kaçırabilir). REST hatası → Err (gizlenmez).
async fn wait_for_open(
    ex: &mut engine::Executor,
    coin: &str,
    target_oid: i64,
    tries: usize,
) -> Result<Option<i64>> {
    for n in 1..=tries {
        let rows = ex
            .refresh_open_orders_all()
            .await
            .context("smoke: açık emir listesi sorgusu başarısız")?;
        if let Some(r) = rows.iter().find(|r| r.oid == target_oid && r.coin == coin) {
            return Ok(Some(r.oid));
        }
        if n < tries {
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        }
    }
    Ok(None)
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("TESTNET").as_deref() != Ok("1") {
        anyhow::bail!("TESTNET=1 DEĞİL — probe_smoke TESTNET dışında çalışmaz");
    }
    let args: Vec<String> = std::env::args().collect();
    let coin = args.get(1).cloned().unwrap_or_else(|| "SUI".into());
    // $20: 0.70 bantta etkin $14 — borsa min $10 üstü (emir GERÇEKTEN gider)
    let notional: f64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(20.0);

    let cfg = Config::from_env()?;
    let info = Info::new(cfg.clone())?;
    let meta = info.meta().await?;
    let idx = meta
        .universe
        .iter()
        .position(|m| m.name == coin)
        .ok_or_else(|| anyhow::anyhow!("coin yok: {coin}"))?;
    println!("[SMOKE] coin={coin} assetIdx={idx} (testnet)");

    // Orta fiyat öğren (borsa mids)
    let mids = info.all_mids().await?;
    let mid: f64 = mids
        .get(&coin)
        .and_then(|m: &String| m.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("mid yok"))?;
    if !(mid.is_finite() && mid > 0.0) {
        anyhow::bail!("mid geçersiz");
    }
    println!("[SMOKE] mid={mid}");

    // Executor ile küçük limit: fiyat 30% DİŞ bantta → kabullenir, DOLMAZ ✓
    let limit_px = hyperliquid_api::perp_price(mid * 0.70, 2);
    println!("[SMOKE] limit px={limit_px} (derin bid — dinlenir)");

    let mut ex = engine::Executor::new(cfg).await?;
    let placed = ex
        .place_limit(&coin, true, notional / mid, limit_px, Tif::Gtc, false)
        .await?;
    let Some(info_oi) = placed else {
        // place_limit None döndü: guard (lot/min-notional) VEYA anında dolma.
        // İKİSİ DE: temizlik (coin'in süpriz açık emirlerini iptal) +
        // FLAT doğrulama → sonra çık. (Eskiden kontrolsüz çıkıyordu.)
        println!("[SMOKE] place None döndü (guard/dolma) — temizlik + flat kontrol");
        let rows = ex
            .refresh_open_orders_all()
            .await
            .context("smoke: cleanup sorgusu başarısız")?;
        for r in rows.iter().filter(|r| r.coin == coin) {
            ex.cancel(&coin, r.oid)
                .await
                .with_context(|| format!("smoke: cleanup cancel başarısız oid={}", r.oid))?;
            println!("[SMOKE] cleanup: oid={} iptal edildi", r.oid);
        }
        assert_flat(&mut ex, &coin).await?;
        println!("[SMOKE] temiz ✔ (emir dinlenmedi — place/cancel tam doğrulanamadı)");
        return Ok(());
    };
    // KENDİ OID'miz — coin adı eşleştirmesi yetmez (aynı coine yabancı
    // emir karışabilir). oid=0 (dolma kanıtı olmadan) → coin fallback.
    let target_oid = info_oi.oid;
    println!(
        "[SMOKE] place sonucu: oid={target_oid} sz={} px={}",
        info_oi.sz, info_oi.px
    );

    // 3) açık-listede OID bazında doğrula (retry'li — eventual consistency)
    //    + 4) iptal + iptali doğrula
    let found_oid = wait_for_open(&mut ex, &coin, target_oid, 5).await?;
    if let Some(oid) = found_oid {
        println!("[SMOKE] açık emir DOĞRULANDI: {coin} oid={oid}");
        ex.cancel(&coin, oid).await?;
        println!("[SMOKE] cancel OK");
        // iptalin gerçekte işlendiğini OID bazında tekrar kontrol et
        let rows2 = ex
            .refresh_open_orders_all()
            .await
            .context("smoke: iptal doğrulama sorgusu başarısız")?;
        if rows2.iter().any(|r2| r2.oid == oid) {
            anyhow::bail!("cancel sonrası oid={oid} hâlâ açık — smoke FİGÜR hata");
        }
        println!("[SMOKE] iptal DOĞRULANDI (oid listede yok) ✓");
    } else {
        // OID listelenmiyor → orderStatus ile SON DURUMU sor (dürüst kanıt:
        // rejected ise emir YERLEŞTİRİLEMEMİŞ demektir → smoke BAŞARISIZ).
        let addr = ex.user().to_string();
        let st = info
            .order_status(&addr, target_oid)
            .await
            .context("smoke: orderStatus sorgusu başarısız")?;
        let inner = st
            .get("order")
            .and_then(|o| o.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("bilinmiyor");
        if inner == "rejected" {
            anyhow::bail!("emir REDDEDİLDİ (orderStatus=rejected) — bakiye/marj kontrol et; smoke kanıt üretemedi");
        }
        anyhow::bail!(
            "oid={target_oid} {tries} denemede açık listede görünmedi (orderStatus={inner}) — place/cancel kanıtı tamamlanamadı",
            tries = 5
        );
    }

    // 5) pozisyon FLAT — REST hatası burada FAIL üretir
    assert_flat(&mut ex, &coin).await?;
    println!("[SMOKE] temiz ✔ (flat doğrulandı)");
    Ok(())
}
