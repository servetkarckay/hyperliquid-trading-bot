//! trail_report — MFE/MAE epoch raporu (Section C k-revizyon protokolü).
//!
//! `cargo run --bin trail_report -- config/trade_exits.jsonl 2026-10-03-a`
//!
//! Okur: trade_exits.jsonl (A2 JSONL), epoch-filtreli tablo basar.
//! Çıkış: MFE/MAE quartile'lar, çıkış sebebi dağılımı, ratchet dağılımı,
//! geri-verme oranı, ilk-stop oranı.

use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct TradeExit {
    ts: u64,
    epoch: String,
    git: String,
    coin: String,
    side: String,
    strategies: Vec<String>,
    mfe_r: f64,
    mae_r: f64,
    dur_sn: u64,
    ratchet: u32,
    cikis: String,
    r_kapanis: f64,
    atr_bps: f64,
    k: f64,
}

fn percentile(mut v: Vec<f64>, p: f64) -> Option<f64> {
    if v.is_empty() { return None; }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((v.len() - 1) as f64 * p).round() as usize;
    Some(v[idx])
}

fn median(v: Vec<f64>) -> Option<f64> {
    percentile(v, 0.5)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Kullanım: trail_report <trade_exits.jsonl> [epoch]");
        std::process::exit(1);
    }
    let path = &args[1];
    let epoch_filter = args.get(2).cloned();

    let f = File::open(path)?;
    let reader = BufReader::new(f);
    let mut exits: Vec<TradeExit> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        if let Ok(e) = serde_json::from_str::<TradeExit>(&line) {
            if epoch_filter.as_ref().map(|f| f == &e.epoch).unwrap_or(true) {
                exits.push(e);
            }
        }
    }

    if exits.is_empty() {
        println!("(boş veri — epoch filtresiyle eşleşen kayıt yok)");
        return Ok(());
    }

    let n = exits.len();
    println!("=== TRAIL RAPOR: epoch={} n={} ===",
        epoch_filter.unwrap_or_else(|| "ALL".into()), n);

    // 1) MFE/MAE quartile'lar
    let mfe: Vec<f64> = exits.iter().map(|e| e.mfe_r).collect();
    let mae: Vec<f64> = exits.iter().map(|e| e.mae_r).collect();
    println!("\n--- MFE/MAE (R cinsinden) ---");
    println!("MFE  p25={:.2} p50={:.2} p75={:.2} p90={:.2} max={:.2}",
        percentile(mfe.clone(), 0.25).unwrap_or(0.0),
        median(mfe.clone()).unwrap_or(0.0),
        percentile(mfe.clone(), 0.75).unwrap_or(0.0),
        percentile(mfe.clone(), 0.9).unwrap_or(0.0),
        mfe.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b)));
    println!("MAE  p25={:.2} p50={:.2} p75={:.2} p90={:.2} min={:.2}",
        percentile(mae.clone(), 0.25).unwrap_or(0.0),
        median(mae.clone()).unwrap_or(0.0),
        percentile(mae.clone(), 0.75).unwrap_or(0.0),
        percentile(mae.clone(), 0.9).unwrap_or(0.0),
        mae.iter().fold(f64::INFINITY, |a, &b| a.min(b)));

    // 2) Çıkış sebebi kırılımı
    let mut by_exit: HashMap<String, usize> = HashMap::new();
    for e in &exits { *by_exit.entry(e.cikis.clone()).or_default() += 1; }
    println!("\n--- Çıkış Sebebi ---");
    for (k, v) in by_exit { println!("  {}: {} ({:.1}%)", k, v, v as f64 / n as f64 * 100.0); }

    // 3) İlk-stop oranı (MFE < 0.5R)
    let first_stop = exits.iter().filter(|e| e.mfe_r < 0.5).count();
    println!("\n--- İlk-Stop Oranı (MFE<0.5R) ---");
    println!("  {}/{} = {:.1}%", first_stop, n, first_stop as f64 / n as f64 * 100.0);

    // 4) Ratchet dağılımı
    let mut ratchet_hist: HashMap<u32, usize> = HashMap::new();
    for e in &exits { *ratchet_hist.entry(e.ratchet).or_default() += 1; }
    println!("\n--- Ratchet Dağılımı ---");
    let mut rats: Vec<_> = ratchet_hist.iter().map(|(k, v)| (*k, *v)).collect();
    rats.sort_by_key(|(r, _)| *r);
    for (r, v) in rats { println!("  ratchet={}: {} ({:.1}%)", r, v, v as f64 / n as f64 * 100.0); }

    // 5) Geri-verme oranı (MFE≥1R alt kümesinde)
    let mfe_ge_1: Vec<f64> = exits.iter()
        .filter(|e| e.mfe_r >= 1.0)
        .map(|e| 1.0 - e.r_kapanis / e.mfe_r.max(f64::EPSILON))
        .collect();
    if !mfe_ge_1.is_empty() {
        let giveback_med = median(mfe_ge_1.clone()).unwrap_or(0.0);
        let giveback_gt60 = mfe_ge_1.iter().filter(|x| **x > 0.6).count();
        println!("\n--- Geri-Veri Oranı (MFE≥1R, n={}) ---", mfe_ge_1.len());
        println!("  median: {:.1}%", giveback_med * 100.0);
        println!("  >60%: {}/{} = {:.1}%", giveback_gt60, mfe_ge_1.len(),
            giveback_gt60 as f64 / mfe_ge_1.len() as f64 * 100.0);
    } else {
        println!("\n--- Geri-Veri Oranı --- (MFE≥1R yok)");
    }

    // 6) TP oranı
    let tp_count = exits.iter().filter(|e| e.cikis == "tp").count();
    println!("\n--- TP Oranı ---");
    println!("  {}/{} = {:.1}%", tp_count, n, tp_count as f64 / n as f64 * 100.0);

    // 7) k değeri
    let k_vals: Vec<f64> = exits.iter().map(|e| e.k).collect();
    if let Some(k) = k_vals.first() {
        println!("\n--- k Değeri ---");
        println!("  k = {:.2} (epoch içinde sabit)", k);
    }

    // KARAR KURALLARI (Section C) — bilgilendirme
    println!("\n=== KARAR KURALLARI (bilgilendirme) ===");
    if n < 30 {
        println!("n={} < 30 → KARAR YOK (yetersiz örneklem)", n);
    } else {
        let first_stop_pct = first_stop as f64 / n as f64 * 100.0;
        if first_stop_pct > 70.0 {
            println!("İlk-stop oranı {:.1}% > 70% → trailing değerlendirilemez (sorun giriş/EV floor)", first_stop_pct);
        } else if !mfe_ge_1.is_empty() {
            let giveback_med = median(mfe_ge_1.clone()).unwrap_or(0.0);
            let giveback_gt60 = mfe_ge_1.iter().filter(|x| **x > 0.6).count() as f64 / mfe_ge_1.len() as f64 * 100.0;
            let tp_pct = tp_count as f64 / n as f64 * 100.0;
            if giveback_med > 0.6 {
                println!("Geri-verme median {:.1}% > 60% → k -0.25 önerisi", giveback_med * 100.0);
            } else if giveback_gt60 < 25.0 && tp_pct < 30.0 {
                println!("Geri-verme düşük ({:.1}% >60%), TP oranı {:.1}% < 30% → k +0.25 önerisi", giveback_gt60, tp_pct);
            } else {
                println!("Geri-verme aralıkta ({:.1}% med, {:.1}% >60), TP={:.1}% → k korunur", giveback_med * 100.0, giveback_gt60, tp_pct);
            }
        }
        let ratchet_zero_pct = *ratchet_hist.get(&0).unwrap_or(&0) as f64 / n as f64 * 100.0;
        if ratchet_zero_pct > 90.0 {
            println!("Ratchet=0 oranı {:.1}% > 90% → TRAIL_ON=0 deneyi önerisi (yeni epoch)", ratchet_zero_pct);
        }
    }

    Ok(())
}
