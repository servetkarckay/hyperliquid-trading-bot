//! PROBE PX — testnet fiyat yuvarlama probu (araç binary).
//!
//! `probe_px <COIN> [mid]`: ALO emri atar, anında iptal eder — POZİSYON
//! ÜRETMEZ; borsanın kabul ettiği fiyat/lot hassasiyetini (`px_dec`)
//! keşfeder. Sonuç `config/tick_cache.json` kalıcılığına beslenir.

use anyhow::Result;
use hyperliquid_api::{Config, Exchange, Info, Tif};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let coin = if args.get(1).map(|s| s.starts_with("--")).unwrap_or(false) {
        "ME".to_string()
    } else {
        args.get(1).cloned().unwrap_or_else(|| "ME".into())
    };
    let cfg = Config::from_env()?;
    let info = Info::new(cfg.clone())?;
    let meta = info.meta().await?;
    let (asset, szd, maxlev) = meta
        .universe
        .iter()
        .enumerate()
        .find(|(_, m)| m.name == coin)
        .map(|(i, m)| (i as u32, m.szDecimals, m.maxLeverage))
        .ok_or_else(|| anyhow::anyhow!("bilinmeyen coin: {coin}"))?;

    // -rest: TEK bir GTC emir bırak (defterde asılı kalır). "orphan emir"
    // testi için: bot açılış yetim temizliğinin bunu iptal etmesi gerekir.
    if args.get(1).map(|s| s.as_str()) == Some("--rest") {
        let coin2 = args.get(2).cloned().unwrap_or_else(|| "BTC".into());
        let (a2, sd2, _) = meta
            .universe
            .iter()
            .enumerate()
            .find(|(_, m)| m.name == coin2)
            .map(|(i, m)| (i as u32, m.szDecimals, m.maxLeverage))
            .ok_or_else(|| anyhow::anyhow!("bilinmeyen coin: {coin2}"))?;
        let mids2 = info.all_mids().await?;
        let m2: f64 = mids2
            .get(&coin2)
            .and_then(|m| m.parse().ok())
            .unwrap_or(0.0);
        // alışta, mid'in %5 ALTINDA → asılı kalır (post-only değil, GTC)
        let px2 = hyperliquid_api::round_px_wire(m2 * 0.95);
        let sz2 = hyperliquid_api::round_sz_up(20.0 / m2, sd2);
        let ex2 = Exchange::new(cfg)?;
        let st = ex2.place_limit(a2, true, sz2, px2, Tif::Gtc, false).await?;
        println!(
            "REST {coin2}: px={px2:.6} sz={sz2} oid={:?} err={:?}",
            st.resting_oid(0),
            st.error(0)
        );
        return Ok(());
    }

    // -cancel <ASSET_ID> <OID>: tek emri oid ile iptal et ( yetim temizliği).
    if args.get(1).map(|s| s.as_str()) == Some("--cancel") {
        let a2: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        let oid: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
        let ex2 = Exchange::new(cfg)?;
        let st = ex2.cancel(a2, oid).await?;
        println!(
            "--cancel asset={a2} oid={oid} status={:?} err={:?}",
            st.status(0),
            st.error(0)
        );
        return Ok(());
    }

    // -cloid: **cloid yeniden kullanım testi** (P2-H).
    // place → cancel → AYNI cloid ile tekrar place. Red gelirse cloid
    // emri-ömrü boyunca tek kullanımlıktır → `make_cloid`'a proses-içi
    // monotonik sayaç eklenmeli (idempotency amacı bozulmadan).
    if args.get(1).map(|s| s.as_str()) == Some("--cloid") {
        let cn = args.get(2).cloned().unwrap_or_else(|| "ME".into());
        let (a2, sd2, _) = meta
            .universe
            .iter()
            .enumerate()
            .find(|(_, m)| m.name == cn)
            .map(|(i, m)| (i as u32, m.szDecimals, m.maxLeverage))
            .ok_or_else(|| anyhow::anyhow!("bilinmeyen coin: {cn}"))?;
        let mids2 = info.all_mids().await?;
        let m2: f64 = mids2.get(&cn).and_then(|m| m.parse().ok()).unwrap_or(0.0);
        // ME gibi ucuz coinlerde 6 hane reddediliyor -> 5 significant figure
        let px2 = hyperliquid_api::round_5sig(m2 * 0.95); // alışta -> asili kalir
        let sz2 = hyperliquid_api::round_sz_up(20.0 / m2, sd2);
        let ex2 = Exchange::new(cfg)?;
        let cl = engine::executor::make_cloid(&cn, px2, sz2, 0);
        println!("{cn}: px={px2:.6} sz={sz2} cloid={cl}");

        let attempt = |tag: String, cloid: String| {
            let ex = &ex2;
            let (px2, sz2, a2) = (px2, sz2, a2);
            async move {
                let st = ex
                    .place_limit_cloid(hyperliquid_api::exchange_endpoint::OrderRequestParams {
                        asset: a2,
                        is_buy: true,
                        sz: sz2,
                        limit_px: px2,
                        tif: Tif::Gtc,
                        reduce_only: false,
                        cloid: Some(&cloid),
                    })
                    .await;
                match st {
                    Ok(s) => match (s.resting_oid(0), s.error(0)) {
                        (Some(oid), _) => println!("{tag:22} -> KABUL (oid={oid})"),
                        (None, Some(e)) => println!("{tag:22} -> RED: {e}"),
                        (None, None) => println!("{tag:22} -> belirsiz"),
                    },
                    Err(e) => println!("{tag:22} -> HATA: {e}"),
                }
            }
        };

        attempt("1. place".into(), cl.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = ex2.cancel_by_cloid(a2, &cl).await;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        attempt("2. AYNI cloid".into(), cl.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        let cl_new = engine::executor::make_cloid(&cn, px2, sz2, 1);
        attempt("3. FARKLI cloid".into(), cl_new.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = ex2.cancel_by_cloid(a2, &cl_new).await;
        return Ok(());
    }

    let mid: f64 = match args.get(2).and_then(|s| s.parse::<f64>().ok()) {
        Some(x) if x > 0.0 => x,
        _ => {
            let mids = info.all_mids().await?;
            mids.get(&coin).and_then(|m| m.parse().ok()).unwrap_or(0.0)
        }
    };
    println!("{coin}: asset={asset} szDecimals={szd} maxLeverage={maxlev} mid={mid}");

    let ex = Exchange::new(cfg)?;
    let sz = hyperliquid_api::round_sz_up(200.0 / mid, szd).max(1.0 / 10f64.powi(szd as i32));

    // denenecek yuvarlamalar
    let cands: Vec<(&str, f64)> = vec![
        ("ham (%.10f)", mid * 0.98),
        ("round_5sig", hyperliquid_api::round_5sig(mid * 0.98)),
        (
            "round_px_wire(6 hane)",
            hyperliquid_api::round_px_wire(mid * 0.98),
        ),
        ("5 hane", (mid * 0.98 * 1e5).round() / 1e5),
        ("4 hane", (mid * 0.98 * 1e4).round() / 1e4),
        ("3 hane", (mid * 0.98 * 1e3).round() / 1e3),
        ("2 hane", (mid * 0.98 * 1e2).round() / 1e2),
        ("5 sig (tam)", round5sig_ref(mid * 0.98)),
    ];
    for (name, px) in cands {
        if px <= 0.0 || !px.is_finite() {
            println!("{name:22} px={px:.10} -> GEÇERSİZ (skip)");
            continue;
        }
        let st = ex.place_limit(asset, true, sz, px, Tif::Alo, false).await;
        match st {
            Ok(s) => match (s.resting_oid(0), s.error(0)) {
                (Some(oid), _) => {
                    println!("{name:22} px={px:.10} -> KABUL (oid={oid}), iptal ediliyor");
                    let _ = ex.cancel(asset, oid).await;
                }
                (None, Some(e)) => println!("{name:22} px={px:.10} -> RED: {e}"),
                (None, None) => println!(
                    "{name:22} px={px:.10} -> belirsiz: {}",
                    s.status(0).unwrap_or(&serde_json::Value::Null)
                ),
            },
            Err(e) => println!("{name:22} px={px:.10} -> HATA: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    }
    Ok(())
}
fn round5sig_ref(x: f64) -> f64 {
    let f = 10f64.powf(x.log10().floor() - 4.0);
    (x / f).round() * f
}
