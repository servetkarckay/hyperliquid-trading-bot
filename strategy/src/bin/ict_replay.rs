//! ICT SWEEP + CISD — tarihsel doğrulama koşucusu.
//!
//! Kullanım: `cargo run -p strategy --bin ict_replay -- <candles.json> [ufuk_bar]`
//!
//! Girdi: `{ "COIN": [{t,o,h,l,c,v}, ...], ... }` (1h mumlar, kronolojik).
//! Simülasyon: sinyal kapanışında karar → SONRAKİ bar açılışında giriş;
//! SL/TP plan fiyatlarından; aynı barda ikisi de vurulursa SL (kötümser);
//! ufuk sonunda kapanış fiyatından çıkış. Komisyon: 2 × %0.045 taker.
use data::Candle;
use serde_json::Value;
use strategy::sweep_cisd::{SweepCisd, MIN_BARS};

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
            let Some(p) = SweepCisd::plan(&cs[..=i]) else {
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
    let sum: f64 = stats.iter().map(|s| s.r).sum();
    let mean = sum / n as f64;
    let gmean = stats.iter().map(|s| s.gross).sum::<f64>() / n as f64;
    let riskp = stats.iter().map(|s| s.risk_pct).sum::<f64>() / n as f64;
    let var: f64 =
        stats.iter().map(|s| (s.r - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0).max(1.0);
    let se = (var / n as f64).sqrt();
    let t = if se > 0.0 { mean / se } else { 0.0 };
    let wins = stats.iter().filter(|s| s.r > 0.0).count();
    let gross_win: f64 = stats.iter().filter(|s| s.r > 0.0).map(|s| s.r).sum();
    let gross_loss: f64 = stats.iter().filter(|s| s.r <= 0.0).map(|s| s.r).sum();
    let pf = if gross_loss < 0.0 {
        gross_win / -gross_loss
    } else {
        f64::INFINITY
    };
    let mut cum = 0.0f64;
    let mut peak = 0.0f64;
    let mut dd: f64 = 0.0;
    for s in stats {
        cum += s.r;
        peak = peak.max(cum);
        dd = dd.max(peak - cum);
    }
    println!(
        "\n{label}: n={n} · win={:.1}% · ΣR={sum:+.2} · ortR(net)={mean:+.3} · ortR(gross)={gmean:+.3} · ortRisk%={riskp:.2} · t={t:.2} · PF={pf:.2} · maxDD={dd:.2}R",
        100.0 * wins as f64 / n as f64
    );
    // Coin bazında dağılım
    let mut coins: Vec<(String, usize, f64)> = Vec::new();
    for s in stats {
        match coins.iter_mut().find(|c| c.0 == s.coin) {
            Some(c) => {
                c.1 += 1;
                c.2 += s.r;
            }
            None => coins.push((s.coin.clone(), 1, s.r)),
        }
    }
    coins.sort_by(|a, b| b.2.total_cmp(&a.2));
    let parts: Vec<String> = coins
        .iter()
        .map(|(c, n, r)| format!("{c}:{n}/{r:+.2}"))
        .collect();
    println!("  coin (n/ΣR): {}", parts.join("  "));
}
