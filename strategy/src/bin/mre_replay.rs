//! MRE (MEAN-REVERSION & OVERREACTION ENGINE) — tarihsel doğrulama
//! koşucusu — KESİT FARKINDALIKLI: tüm coinler aynı barda taranır.
//!
//! Kullanım: `cargo run -p strategy --bin mre_replay -- <candles.json> [ufuk_bar]`
//!
//! Katman zinciri `strategy::mre::plan_replay` ile üretimle BİREBİR
//! aynıdır (Z → rejim → GARCH σ → kesit rank); tek fark kesit bantları
//! `rank_band_at` ile hesaplanır — replay veri kümesi 8 coindir ve
//! kanonik ≥20 coin kontratı küçük evrende bilinçli esner (dokümante
//! sapma; eşik araması DEĞİL).
//!
//! Evren getirileri: her barda TÜM coinlerin 24-bar getirisi (engine'in
//! canlı ctx.universe_returns kontratının replay karşılığı).
//! Simülasyon: sinyal barında karar → SONRAKİ bar açılışında giriş;
//! TP = aşırı hareket başlangıcı, SL = 1.5×ATR14; aynı barda ikisi →
//! SL (kötümser); ufuk sonunda kapanıştan çıkış; komisyon 2×%0.045.
//! Coin başına açık işlem kilidi (exit_idx'e kadar yeniden sinyal yok).
use data::Candle;
use serde_json::Value;
use strategy::mre::{plan_replay, MIN_BARS};

fn parse_candles(v: &Value) -> Vec<Candle> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    Some(Candle {
                        t: r.get("t")?.as_i64()?,
                        o: r.get("o")?.as_f64()?,
                        h: r.get("h")?.as_f64()?,
                        l: r.get("l")?.as_f64()?,
                        c: r.get("c")?.as_f64()?,
                        v: r.get("v")?.as_f64().unwrap_or(0.0),
                        n: 0,
                        closed: true,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone)]
struct Stat {
    coin: String,
    long: bool,
    r: f64,
    risk_pct: f64,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "/tmp/opencode/ict_candles.json".into());
    let horizon: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(96);
    let raw = std::fs::read_to_string(&path).expect("candles.json okunamadı");
    let root: Value = serde_json::from_str(&raw).expect("json hatası");
    let obj = root.as_object().expect("obje bekleniyor");
    let coins: Vec<String> = obj.keys().cloned().collect();
    let series: Vec<Vec<Candle>> = coins
        .iter()
        .map(|c| parse_candles(obj.get(c).unwrap()))
        .collect();
    let nmax = series.iter().map(|s| s.len()).max().unwrap_or(0);

    let mut stats: Vec<Stat> = Vec::new();
    let mut until: Vec<usize> = coins.iter().map(|_| 0).collect();
    for i in (MIN_BARS - 1)..nmax {
        // evren 24-bar getirileri — her barda TÜM coinler
        let mut uni: Vec<(String, f64)> = Vec::with_capacity(coins.len());
        for (k, coin) in coins.iter().enumerate() {
            let cs = &series[k];
            if cs.len() > i && i >= 24 {
                let rx = cs[i].c / cs[i - 24].c - 1.0;
                if rx.is_finite() {
                    uni.push((coin.clone(), rx));
                }
            }
        }
        if uni.len() < coins.len() {
            continue;
        }
        for (k, coin) in coins.iter().enumerate() {
            if until[k] > i || series[k].len() <= i + 1 {
                continue;
            }
            let slice = &series[k][..=i];
            let Some(p) = plan_replay(slice, &uni, coin) else {
                continue;
            };
            let entry = series[k][i + 1].o;
            let risk = if p.long { entry - p.sl } else { p.sl - entry };
            if risk.is_nan() || risk <= 0.0 || !risk.is_finite() {
                continue;
            }
            let end = (i + 1 + horizon).min(series[k].len());
            let mut r: Option<f64> = None;
            let mut exit_idx = end - 1;
            for (j, bar) in series[k].iter().enumerate().skip(i + 1).take(end - i - 1) {
                let (sl_hit, tp_hit) = if p.long {
                    (bar.l <= p.sl, bar.h >= p.tp)
                } else {
                    (bar.h >= p.sl, bar.l <= p.tp)
                };
                if sl_hit {
                    r = Some(-1.0);
                    exit_idx = j;
                    break;
                }
                if tp_hit {
                    r = Some((p.tp - entry).abs() / risk);
                    exit_idx = j;
                    break;
                }
            }
            let gross = r.unwrap_or_else(|| {
                let last = &series[k][end - 1];
                if p.long {
                    (last.c - entry) / risk
                } else {
                    (entry - last.c) / risk
                }
            });
            let cost = 0.0009 * entry / risk;
            stats.push(Stat {
                coin: coin.clone(),
                long: p.long,
                r: gross - cost,
                risk_pct: risk / entry,
            });
            until[k] = exit_idx + 1;
        }
    }
    if stats.is_empty() {
        println!("TOPLAM: 0 işlem");
        return;
    }
    let n = stats.len();
    let wins = stats.iter().filter(|s| s.r > 0.0).count();
    let sum: f64 = stats.iter().map(|s| s.r).sum();
    let mut curve = 0.0f64;
    let mut peak = 0.0f64;
    let mut mdd = 0.0f64;
    for s in &stats {
        curve += s.r;
        peak = peak.max(curve);
        mdd = mdd.max(peak - curve);
    }
    println!(
        "TOPLAM: {n} işlem · win %{:.0} · ΣR {:.2} · ort {:.3}R · MDD {mdd:.1}R",
        100.0 * wins as f64 / n as f64,
        sum,
        sum / n as f64
    );
    for s in stats.iter().take(12) {
        println!(
            "  {} {} · {:.2}R (risk %{:.2})",
            s.coin,
            if s.long { "LONG" } else { "SHORT" },
            s.r,
            100.0 * s.risk_pct
        );
    }
}
