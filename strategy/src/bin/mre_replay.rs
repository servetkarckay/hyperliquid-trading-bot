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
use math::indicators;
use serde_json::Value;
use strategy::mre::{plan_replay, MIN_BARS};
use strategy::regime_detection::classify;
use strategy::MarketInput;

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
    // ── giriş-anı özellikleri (veto analizi) ──
    /// son bar hacmi, önceki 20 bara göre z-skoru
    vol_z: Option<f64>,
    /// yana hizalı 24s getiri (%): SHORT için +ret24, LONG için −ret24
    side_move: Option<f64>,
    /// ATR14/close (%)
    atr_pct: Option<f64>,
    /// sinyal barı üst-süpürme: 20 barın tepeğini fitille aşıp kırmızı kapanış
    sweep_hi: bool,
}

fn summarize(name: &str, sel: &[&Stat]) {
    if sel.is_empty() {
        println!("{name:>28}: 0 işlem");
        return;
    }
    let n = sel.len();
    let wins = sel.iter().filter(|s| s.r > 0.0).count();
    let sum: f64 = sel.iter().map(|s| s.r).sum();
    let gross_w: f64 = sel.iter().filter(|s| s.r > 0.0).map(|s| s.r).sum();
    let gross_l: f64 = sel.iter().filter(|s| s.r <= 0.0).map(|s| -s.r).sum();
    let pf = if gross_l > 0.0 {
        gross_w / gross_l
    } else {
        f64::INFINITY
    };
    println!(
        "{name:>28}: {n:>4} işlem · win %{:.0} · ΣR {:>7.2} · ort {:+.3}R · pf {:.2}",
        100.0 * wins as f64 / n as f64,
        sum,
        sum / n as f64,
        pf
    );
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
    let mut dump: Vec<serde_json::Value> = Vec::new();
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
        // kanonik kontrat: ≥20 coin sıralaması güvenilir; yeni liste
        // kısa-seri coinler (ör. XTEN) tüm barları kilitlemesin
        if uni.len() < 20 {
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
            // ── giriş-anı özellikler (sinyal barı i) ──
            let cs_k = &series[k];
            let vol_z = if i >= 21 {
                let win = &cs_k[i - 20..i];
                let m = win.iter().map(|c| c.v).sum::<f64>() / 20.0;
                let sd = (win.iter().map(|c| (c.v - m) * (c.v - m)).sum::<f64>() / 20.0).sqrt();
                (sd > 1e-12).then(|| (cs_k[i].v - m) / sd)
            } else {
                None
            };
            let ret24 = (i >= 24).then(|| (cs_k[i].c / cs_k[i - 24].c - 1.0) * 100.0);
            let side_move = ret24.map(|x| if p.long { -x } else { x });
            let atr_pct = {
                let n_atr = 14.min(cs_k.len() - 1);
                let mut tr_sum = 0.0f64;
                for jj in 0..n_atr {
                    let cur = &cs_k[i - n_atr + 1 + jj];
                    let prev_c = cs_k[i - n_atr + jj].c;
                    let tr = (cur.h - cur.l)
                        .max((cur.h - prev_c).abs())
                        .max((cur.l - prev_c).abs());
                    tr_sum += tr;
                }
                let tr = tr_sum / n_atr as f64;
                (cs_k[i].c > 0.0).then(|| tr / cs_k[i].c * 100.0)
            };
            let sweep_hi = i >= 21 && {
                let hh = cs_k[i - 20..i]
                    .iter()
                    .map(|c| c.h)
                    .fold(f64::NEG_INFINITY, f64::max);
                cs_k[i].h > hh && cs_k[i].c < cs_k[i].o
            };
            stats.push(Stat {
                coin: coin.clone(),
                long: p.long,
                r: gross - cost,
                risk_pct: risk / entry,
                vol_z,
                side_move,
                atr_pct,
                sweep_hi,
            });
            // ── TAM GÖSTERGE MADENCİLİĞİ dökümü (sinyal barı i) ──
            let cs = &series[k][..=i];
            type Series = Vec<f64>;
            let (oo, hh, ll, cc, vv): (Series, Series, Series, Series, Series) = (
                cs.iter().map(|c| c.o).collect(),
                cs.iter().map(|c| c.h).collect(),
                cs.iter().map(|c| c.l).collect(),
                cs.iter().map(|c| c.c).collect(),
                cs.iter().map(|c| c.v).collect(),
            );
            let m = i; // son (sinyal) bar indeksi
            let atr_abs = indicators::atr(&hh, &ll, &cc, 14).get(m).copied().flatten();
            let cpx = cc[m];
            let f_rsi = |n: usize| indicators::rsi(&cc, n).get(m).copied().flatten();
            let (_, _, macd_hist) = indicators::macd(&cc, 12, 26, 9);
            let macd_hist = macd_hist.get(m).copied().flatten();
            let (ema20, ema50) = (
                indicators::ema(&cc, 20).get(m).copied().flatten(),
                indicators::ema(&cc, 50).get(m).copied().flatten(),
            );
            let (bb_mid, bb_hi, bb_lo) = match indicators::bollinger(&cc, 20, 2.0).get(m).copied() {
                Some((a, b, c3)) => (a, b, c3),
                None => (None, None, None),
            };
            let (stoch_k, _) = indicators::stochastic(&hh, &ll, &cc, 14, 3);
            let stoch_k = stoch_k.get(m).copied().flatten();
            let (_, _, adx_v) = indicators::adx(&hh, &ll, &cc, 14);
            let adx_v = adx_v.get(m).copied().flatten();
            let obv = indicators::obv(&cc, &vv);
            let obv_flow = if m >= 20 && vv[m - 19..=m].iter().sum::<f64>() > 0.0 {
                Some((obv[m] - obv[m - 20]) / vv[m - 19..=m].iter().sum::<f64>())
            } else {
                None
            };
            let vw = indicators::rolling_vwap(&hh, &ll, &cc, &vv, 24);
            let vw = vw.get(m).copied().flatten();
            let vwap_gap = vw.filter(|x| *x > 0.0).map(|x| (cpx / x - 1.0) * 100.0);
            let rng = (hh[m] - ll[m]).max(1e-12);
            let up_wick = (hh[m] - oo[m].max(cc[m])) / rng * 100.0;
            let dn_wick = (oo[m].min(cc[m]) - ll[m]) / rng * 100.0;
            let body_pct = (cc[m] - oo[m]).abs() / rng * 100.0;
            let range_atr = atr_abs.map(|a| rng / a);
            let hh72 = hh[hh.len().saturating_sub(72)..]
                .iter()
                .cloned()
                .fold(f64::MIN, f64::max);
            let ll72 = ll[ll.len().saturating_sub(72)..]
                .iter()
                .cloned()
                .fold(f64::MAX, f64::min);
            let from_hi72 = (cpx / hh72 - 1.0) * 100.0;
            let from_lo72 = (cpx / ll72 - 1.0) * 100.0;
            let ret6 = if m >= 6 && cc[m - 6] > 0.0 {
                Some((cpx / cc[m - 6] - 1.0) * 100.0)
            } else {
                None
            };
            let m20 =
                cc[cc.len().saturating_sub(20)..].iter().sum::<f64>() / cc.len().min(20) as f64;
            let w20 = &cc[cc.len().saturating_sub(20)..];
            let sd20 =
                (w20.iter().map(|x| (x - m20) * (x - m20)).sum::<f64>() / w20.len() as f64).sqrt();
            let z20 = if sd20 > 1e-12 {
                Some((cpx - m20) / sd20)
            } else {
                None
            };
            let mut consec = 0i64;
            for j in (1..=m).rev() {
                let dir_against = if p.long {
                    cc[j] > cc[j - 1]
                } else {
                    cc[j] < cc[j - 1]
                };
                if dir_against {
                    consec += 1
                } else {
                    break;
                }
            }
            let mut med50: Vec<f64> = vv[vv.len().saturating_sub(50)..].to_vec();
            med50.sort_by(|a, b| a.total_cmp(b));
            let vol_med50 = med50.get(med50.len() / 2).copied().unwrap_or(0.0);
            let vol_med_ratio = if vol_med50 > 0.0 {
                Some(vv[m] / vol_med50)
            } else {
                None
            };
            // sweep_lo: 20 bar dibi fitille kırılıp yeşil kapanış
            let ll20 = ll[ll.len().saturating_sub(21)..ll.len() - 1]
                .iter()
                .cloned()
                .fold(f64::MAX, f64::min);
            let sweep_lo = ll[m] < ll20 && cc[m] > oo[m];
            let hh20 = hh[hh.len().saturating_sub(21)..hh.len() - 1]
                .iter()
                .cloned()
                .fold(f64::MIN, f64::max);
            let sweep_hi2 = hh[m] > hh20 && cc[m] < oo[m];
            let regime = {
                let input = MarketInput {
                    coin: String::new(),
                    candles: cs.to_vec(),
                    ..MarketInput::default()
                };
                classify(&input)
                    .map(|(lb, _)| format!("{lb:?}"))
                    .unwrap_or_default()
            };
            let entry_ts = series[k][i + 1].t;
            let bb_pctb = match (bb_lo, bb_hi) {
                (Some(l), Some(h2)) if h2 > l => Some((cpx - l) / (h2 - l)),
                _ => None,
            };
            let bb_bw = match (bb_lo, bb_hi, bb_mid) {
                (Some(l), Some(h2), Some(mid)) if mid > 0.0 => Some((h2 - l) / mid),
                _ => None,
            };
            let opt = |x: Option<f64>| {
                x.map(serde_json::Value::from)
                    .unwrap_or(serde_json::Value::Null)
            };
            let mut row = serde_json::Map::new();
            row.insert("coin".into(), serde_json::Value::from(coin.as_str()));
            row.insert("long".into(), serde_json::Value::from(p.long));
            row.insert("r".into(), serde_json::Value::from(gross - cost));
            row.insert("risk_pct".into(), serde_json::Value::from(risk / entry));
            row.insert("entry_ts".into(), serde_json::Value::from(entry_ts));
            row.insert("rsi14".into(), opt(f_rsi(14)));
            row.insert("rsi2".into(), opt(f_rsi(2)));
            row.insert(
                "macd_hist_pct".into(),
                opt(macd_hist.map(|x| x / cpx * 100.0)),
            );
            row.insert(
                "ema20_atr".into(),
                opt(atr_abs.zip(ema20).map(|(a, e)| (cpx - e) / a)),
            );
            row.insert(
                "ema50_atr".into(),
                opt(atr_abs.zip(ema50).map(|(a, e)| (cpx - e) / a)),
            );
            row.insert("bb_pctb".into(), opt(bb_pctb));
            row.insert("bb_bw".into(), opt(bb_bw));
            row.insert("stoch_k".into(), opt(stoch_k));
            row.insert("adx".into(), opt(adx_v));
            row.insert("obv_flow".into(), opt(obv_flow));
            row.insert("vwap_gap".into(), opt(vwap_gap));
            row.insert("up_wick".into(), serde_json::Value::from(up_wick));
            row.insert("dn_wick".into(), serde_json::Value::from(dn_wick));
            row.insert("body_pct".into(), serde_json::Value::from(body_pct));
            row.insert("range_atr".into(), opt(range_atr));
            row.insert("from_hi72".into(), serde_json::Value::from(from_hi72));
            row.insert("from_lo72".into(), serde_json::Value::from(from_lo72));
            row.insert("ret6".into(), opt(ret6));
            row.insert("ret24".into(), opt(ret24));
            row.insert("side_move".into(), opt(side_move));
            row.insert("z20".into(), opt(z20));
            row.insert("consec_against".into(), serde_json::Value::from(consec));
            row.insert("vol_z".into(), opt(vol_z));
            row.insert("vol_med_ratio".into(), opt(vol_med_ratio));
            row.insert("atr_pct".into(), opt(atr_pct));
            row.insert("sweep_hi".into(), serde_json::Value::from(sweep_hi2));
            row.insert("sweep_lo".into(), serde_json::Value::from(sweep_lo));
            row.insert("regime".into(), serde_json::Value::from(regime.as_str()));
            row.insert(
                "hour_utc".into(),
                serde_json::Value::from((entry_ts / 3_600_000) % 24),
            );
            dump.push(serde_json::Value::Object(row));
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

    // vetolar env ile ayarlanabilir (sağlamlık taraması için)
    let veto_volz: f64 = std::env::var("VETO_VOLZ")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.5);
    let veto_move: f64 = std::env::var("VETO_MOVE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5.0);
    let veto_volz: f64 = veto_volz.clamp(0.0, 50.0);
    let veto_move: f64 = veto_move.clamp(0.0, 100.0);
    println!("\n── VETO KIRILIMI (eşikler: vol_z<={veto_volz}, side_move<=%{veto_move}) ──");
    let ok_volz = |s: &Stat| s.vol_z.map(|z| z <= veto_volz).unwrap_or(true);
    let ok_move = |s: &Stat| s.side_move.map(|m| m <= veto_move).unwrap_or(true);
    summarize("BASELINE (filtre yok)", &stats.iter().collect::<Vec<_>>());
    summarize(
        "veto geçen: vol_z≤1.5",
        &stats.iter().filter(|s| ok_volz(s)).collect::<Vec<_>>(),
    );
    summarize(
        "veto RED: vol_z>1.5",
        &stats.iter().filter(|s| !ok_volz(s)).collect::<Vec<_>>(),
    );
    summarize(
        "veto geçen: move≤%5",
        &stats.iter().filter(|s| ok_move(s)).collect::<Vec<_>>(),
    );
    summarize(
        "veto RED: move>%5",
        &stats.iter().filter(|s| !ok_move(s)).collect::<Vec<_>>(),
    );
    summarize(
        "ikisi birden (AND)",
        &stats
            .iter()
            .filter(|s| ok_volz(s) && ok_move(s))
            .collect::<Vec<_>>(),
    );
    summarize(
        "ikisinden biri RED",
        &stats
            .iter()
            .filter(|s| !(ok_volz(s) && ok_move(s)))
            .collect::<Vec<_>>(),
    );

    // Özellik ortalamaları: kazanan vs kaybeden (canlı örneklemin aynısı)
    let w: Vec<&Stat> = stats.iter().filter(|s| s.r > 0.0).collect();
    let l: Vec<&Stat> = stats.iter().filter(|s| s.r <= 0.0).collect();
    let avg_vz = |v: &[&Stat]| -> Option<f64> {
        let xs: Vec<f64> = v.iter().filter_map(|s| s.vol_z).collect();
        (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
    };
    let avg_mv = |v: &[&Stat]| -> Option<f64> {
        let xs: Vec<f64> = v.iter().filter_map(|s| s.side_move).collect();
        (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
    };
    let avg_atr = |v: &[&Stat]| -> Option<f64> {
        let xs: Vec<f64> = v.iter().filter_map(|s| s.atr_pct).collect();
        (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
    };
    println!(
        "  ort      vol_z: W={:.2}  L={:.2}\n  ort side_move: W={:.2}  L={:.2}\n  ort    atr_pct: W={:.2}  L={:.2}",
        avg_vz(&w).unwrap_or(f64::NAN),
        avg_vz(&l).unwrap_or(f64::NAN),
        avg_mv(&w).unwrap_or(f64::NAN),
        avg_mv(&l).unwrap_or(f64::NAN),
        avg_atr(&w).unwrap_or(f64::NAN),
        avg_atr(&l).unwrap_or(f64::NAN)
    );
    let sw_w = w.iter().filter(|s| s.sweep_hi).count();
    let sw_l = l.iter().filter(|s| s.sweep_hi).count();
    println!(
        "  üst-süpürme oranı: W {}/{} · L {}/{}",
        sw_w,
        w.len(),
        sw_l,
        l.len()
    );

    // işlem dökümü: temel (stats) + zengin (madencilik, `dump`) ayrı dosyalara
    let basic: Vec<serde_json::Value> = stats
        .iter()
        .map(|s| {
            serde_json::json!({
                "coin": s.coin, "long": s.long, "r": s.r, "risk_pct": s.risk_pct,
                "vol_z": s.vol_z, "side_move": s.side_move, "atr_pct": s.atr_pct,
                "sweep_hi": s.sweep_hi,
            })
        })
        .collect();
    let _ = std::fs::write(
        "/tmp/opencode/mre_backtest_trades.json",
        serde_json::to_string_pretty(&basic).unwrap_or_default(),
    );
    let _ = std::fs::write(
        "/tmp/opencode/mre_feature_dump.json",
        serde_json::to_string_pretty(&dump).unwrap_or_default(),
    );
}
