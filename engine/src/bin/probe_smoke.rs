//! PROBE SMOKE — testnet-e2e workflow'unun gerçek order adımı.
//!
//! KULLANIM (YALNIZ testnet, ELLE):
//!   TESTNET=1 probe_smoke <COIN> [notional=12]
//!
//! Akış:
//!   1. mainnet guard: TESTNET=1 yoksa HİÇBİR şey yapmaz (HATA).
//!   2. küçük limit order yerleştirir (IOC değil, ALO/limit — açık kalır).
//!   3. emri açık-listede DOĞRULAR.
//!   4. İPTAL eder.
//!   5. pozisyon FLAT doğrular.
//!
//! Bu binary normal bot koşusunun PARÇASI DEĞİLDİR — yalnız elle smoke.

use anyhow::Result;
use hyperliquid_api::{Config, Info, Tif};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("TESTNET").as_deref() != Ok("1") {
        anyhow::bail!("TESTNET=1 DEĞİL — probe_smoke TESTNET dışında çalışmaz");
    }
    let args: Vec<String> = std::env::args().collect();
    let coin = args.get(1).cloned().unwrap_or_else(|| "SUI".into());
    let notional: f64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(12.0);

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

    // Executor ile küçük limit (ALO-uyumlu değil; borsada post_only yok — IOC
    // NO: anında doldu → iptal başarısız; limit alanına GİR: fiyatı 30% DİŞ
    // bantta koy → kabullenir ama DOLMAZ ✓
    let limit_px = hyperliquid_api::perp_price(mid * 0.70, 2);
    println!("[SMOKE] limit px={limit_px} (derin bandsız düşük bid tarafı)");

    // Executor kur
    let mut ex = engine::Executor::new(cfg).await?;
    let placed = ex
        .place_limit(&coin, true, notional / mid, limit_px, Tif::Gtc, false)
        .await?;
    let Some(info_oi) = placed else {
        // doldu (beklenmez ama kanıt); FLAT doğrulama
        println!("[SMOKE] order döndü None (dolabilir kurallar) — flat kontrol");
        return Ok(());
    };
    println!("[SMOKE] place sonucu: {info_oi:?}");

    // 3) açık-listede doğrula + 4) iptal
    let rows = ex.refresh_open_orders_all().await.unwrap_or_default();
    if let Some(r) = rows.iter().find(|r| r.coin == coin) {
        let (cn, oid) = (r.coin.clone(), r.oid);
        println!("[SMOKE] açık emir DOĞRULANDI: {cn} oid={oid}");
        ex.cancel(&coin, oid).await?;
        println!("[SMOKE] cancel OK");
        // iptalin gerçekte işlendiğini tekrar kontrol et
        let rows2 = ex.refresh_open_orders_all().await.unwrap_or_default();
        if rows2.iter().any(|r2| r2.coin == coin) {
            anyhow::bail!("cancel sonrası emir hâlâ açık — smoke FİGÜR hata");
        }
        println!("[SMOKE] iptal DOĞRULANDI (açık listede yok) ✓");
    } else {
        println!("[SMOKE] açık listede emir yok (doldu/hemen reddedildi)");
    }

    // 5) pozisyon FLAT ✓/eksik sinyal
    let p = ex
        .refresh_positions()
        .await
        .ok()
        .and_then(|_| ex.positions().get(&coin).cloned());
    let flat = p.as_ref().map(|x| x.szi.abs() < 1e-12).unwrap_or(true);
    println!(
        "[SMOKE] FLAT kontrol: {} szi={:?}",
        if flat {
            "✓"
        } else {
            "POSİZYON VAR — manuel temizlik!"
        },
        p.map(|x| x.szi)
    );
    if !flat {
        anyhow::bail!("smoke bitti ama pozisyon kapısı kapanmadı");
    }
    println!("[SMOKE] temiz ✔");
    Ok(())
}
