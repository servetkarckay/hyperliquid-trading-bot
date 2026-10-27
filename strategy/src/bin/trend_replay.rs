//! MMT (MACRO-MICRO TREND ENGINE) — tarihsel doğrulama koşucusu.
//!
//! Kullanım: `cargo run -p strategy --bin trend_replay -- <candles.json> [ufuk_bar]`
//!
//! Girdi: `{ "COIN": [{t,o,h,l,c,v}, ...], ... }` (1h mumlar, kronolojik).
//! Simülasyon: sinyal kapanışında karar → SONRAKİ bar açılışında giriş;
//! SL/TP plan fiyatlarından (SL = 2N, TP = 3N); aynı barda ikisi de
//! vurulursa SL (kötümser); ufuk sonunda kapanış fiyatından çıkış.
//! Komisyon: 2 × %0.045 taker.
use data::Candle;
use serde_json::Value;
use strategy::mmt::{Mmt, MIN_BARS};

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
    gross: f64,
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

    let mut stats: Vec<Stat> = Vec::new();
    for (coin, val) in obj {
        let cs = parse_candles(val);
        if cs.len() < MIN_BARS + 2 {
            println!("{coin}: yetersiz veri ({} bar)", cs.len());
            continue;
        }
        let mut i = MIN_BARS - 1;
        let mut n_coin = 0;
        while i + 1 < cs.len() {
            let Some(p) = Mmt::plan(&cs[..=i]) else {
                i += 1;
                continue;
            };
            let entry = cs[i + 1].o;
            let risk = if p.long { entry - p.sl } else { p.sl - entry };
            if risk.is_nan() || risk <= 0.0 || !risk.is_finite() {
                i += 1;
                continue;
            }
            let end = (i + 1 + horizon).min(cs.len());
            let mut r: Option<f64> = None;
            let mut exit_idx = end - 1;
            for (j, bar) in cs.iter().enumerate().skip(i + 1).take(end - i - 1) {
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
                let last = &cs[end - 1];
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
                gross,
                risk_pct: risk / entry,
            });
            n_coin += 1;
            i = exit_idx + 1;
        }
        println!("{coin}: {} bar · {} işlem", cs.len(), n_coin);
    }

    if stats.is_empty() {
        println!("\nTOPLAM: 0 işlem");
        return;
    }
    report(&stats, "TOPLAM");
    report(
        &stats.iter().filter(|s| s.long).cloned().collect::<Vec<_>>(),
        "LONG",
    );
    report(
        &stats
            .iter()
            .filter(|s| !s.long)
            .cloned()
            .collect::<Vec<_>>(),
        "SHORT",
    );
}

fn report(stats: &[Stat], label: &str) {
    let n = stats.len();
    if n == 0 {
        println!("\n{label}: 0 işlem");
        return;
    }
    let wins = stats.iter().filter(|s| s.r > 0.0).count();
    let sum: f64 = stats.iter().map(|s| s.r).sum();
    let gross: f64 = stats.iter().map(|s| s.gross).sum();
    let mut worst = f64::INFINITY;
    let mut best = f64::NEG_INFINITY;
    let mut max_dd: f64 = 0.0;
    let mut curve: f64 = 0.0;
    let mut peak: f64 = 0.0;
    for s in stats {
        curve += s.r;
        peak = peak.max(curve);
        max_dd = max_dd.max(peak - curve);
        worst = worst.min(s.r);
        best = best.max(s.r);
    }
    println!(
        "\n{label}: {n} işlem · win %{:.0} · ΣR {:.2} · net/R ort {:.3} (gross {:.2}) · best {:.1}R · worst {:.1}R · MDD {:.1}R",
        100.0 * wins as f64 / n as f64,
        sum,
        sum / n as f64,
        gross,
        best,
        worst,
        max_dd
    );
    let mut yorum = false;
    for s in stats.iter().take(8) {
        println!(
            "  {} {} · {:.2}R (risk % {:.2})",
            s.coin,
            if s.long { "LONG" } else { "SHORT" },
            s.r,
            100.0 * s.risk_pct
        );
        yorum = true;
    }
    let _ = yorum;
}
