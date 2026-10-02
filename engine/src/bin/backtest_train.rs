//! backtest_train — KELLY EĞİTİMİ (tüm aktif evren, thread-paralel):
//! canlı API'den 1h × 180g mum çeker, HER stratejiyi IS/OOS'da replay eder,
//! sharpe-temelli ensemble ağırlıklarını üretir → config/ensemble_weights.json.
//!
//! HIZ MİMARİSİ: (a) bar-major: MarketInput bar başına 1 kez kurulur
//! (18 strateji paylaşır — 18× daha az klon); (b) coin parçaları
//! thread'lere dağıtılır (çekirdek sayısı kadar).

use std::collections::HashMap;
use std::sync::Arc;

use data::backfill_job::{pump_candles_from, CandleSink};
use engine::backtest::BtConfig;
use engine::backtest_multi::run_multi;
use hyperliquid_api::{Config, Info};
use strategy::registry;
use utils::logger;

/// bellek-içi sink: (coin, iv) → Vec<Candle>
struct VecSink {
    map: HashMap<(String, u64), Vec<data::Candle>>,
}

#[async_trait::async_trait]
impl CandleSink for VecSink {
    async fn write(
        &mut self,
        coin: &str,
        interval_secs: u64,
        candles: &[hyperliquid_api::types::Candle],
    ) -> anyhow::Result<usize> {
        let entry = self.map.entry((coin.to_string(), interval_secs)).or_default();
        for c in candles {
            entry.push(data::Candle {
                t: c.t,
                o: c.o.parse().unwrap_or(0.0),
                h: c.h.parse().unwrap_or(0.0),
                l: c.l.parse().unwrap_or(0.0),
                c: c.c.parse().unwrap_or(0.0),
                v: c.v.parse().unwrap_or(0.0),
                n: c.n.map(|x| x.max(0) as u64).unwrap_or(0),
                closed: true,
            });
        }
        Ok(candles.len())
    }
}

#[derive(Default, Clone)]
struct AggAcc {
    coins: usize,
    trades: u64,
    sharpe_sum: f64,
    pf_sum: f64,
    wr_sum: f64,
    oos_pf_sum: f64,
    oos_sharpe_sum: f64,
    overfit_coins: usize,
    thin_coins: usize,
    // IS
    is_wr: f64, is_ret: f64, is_mdd: f64, is_bars: f64,
    // OOS
    oos_wr: f64, oos_ret: f64, oos_mdd: f64, oos_bars: f64, oos_trades: u64,
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logger::init();

    let cfg = Config::from_env()?;
    let info = Info::new(cfg.clone())?;

    // VARSAYILAN: TÜM AKTİF EVREN (178 coin) — kısıtlama için BT_COINS="BTC,ETH"
    let coins: Vec<String> = match std::env::var("BT_COINS") {
        Ok(s) if !s.is_empty() => s.split(',').map(|x| x.trim().to_string()).collect(),
        _ => {
            let icfg = data::IngestorConfig { l2book_top_n: 0, ..Default::default() };
            let ing = data::Ingestor::new(cfg.clone(), icfg).await?;
            ing.universe.names()
        }
    };
    let days: u64 = std::env::var("BT_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(180);
    let interval: u64 = std::env::var("BT_INTERVAL").ok().and_then(|v| v.parse().ok()).unwrap_or(3600);

    tracing::info!("KELLY EĞİTİMİ: {} coin × {days}g × {interval}s", coins.len());
    let t0 = std::time::Instant::now();

    // 1) VERİ ÇEK (paginasyonlu, 429-retry'li)
    let now = hyperliquid_api::now_ms() as i64;
    let start = now - days as i64 * 86_400_000;
    let mut sink = VecSink { map: HashMap::new() };
    for coin in &coins {
        let n = pump_candles_from(&info, coin, interval, start, now, 500, &mut sink).await?;
        tracing::info!("{coin}: {n} mum");
    }
    let fetched: Arc<HashMap<(String, u64), Vec<data::Candle>>> = Arc::new(sink.map);
    tracing::info!("veri haz: {:.1}s", t0.elapsed().as_secs_f32());

    // 2) EVREN GETİRİSİ (kesit momentum beslemesi) — sağdan hizalı 7-bar
    let anchor: Arc<HashMap<String, Vec<f64>>> = Arc::new(
        fetched
            .iter()
            .map(|((c, _), candles)| (c.clone(), candles.iter().map(|x| x.c).collect()))
            .collect(),
    );
    let longest = anchor.values().map(|v| v.len()).max().unwrap_or(0);

    // 3) PARALEL REPLAY: coin parçaları → thread'ler
    let strategies: Arc<Vec<Box<dyn strategy::Strategy>>> = Arc::new(registry());
    let bt_cfg = BtConfig { interval_secs: interval, days, ..Default::default() };
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let chunk_size = coins.len().div_ceil(workers);
    type Row = (engine::backtest::StrategyPerformance, engine::backtest::StrategyPerformance);
    let (txp, rxp) = std::sync::mpsc::channel::<Row>();

    let mut handles = Vec::new();
    for chunk in coins.chunks(chunk_size) {
        let chunk = chunk.to_vec();
        let strategies_c = strategies.clone();
        let fetched_c = fetched.clone();
        let anchor_c = anchor.clone();
        let txp_c = txp.clone();
        let bt_cfg_c = bt_cfg.clone();
        handles.push(std::thread::spawn(move || {
            for coin in &chunk {
                let candles = match fetched_c.get(&(coin.clone(), interval)) {
                    Some(c) if c.len() > 120 => c.clone(),
                    _ => continue,
                };
                let returns_at = |i: usize| -> Vec<(String, f64)> {
                    let mut out: Vec<(String, f64)> = Vec::with_capacity(anchor_c.len());
                    for (cn, cc) in anchor_c.iter() {
                        let ii = i as i64 - (longest as i64 - cc.len() as i64);
                        if ii >= 7 {
                            let ii = ii as usize;
                            out.push((cn.clone(), cc[ii] / cc[ii - 7] - 1.0));
                        }
                    }
                    out
                };
                for row in run_multi(&strategies_c, coin, &candles, &returns_at, &bt_cfg_c) {
                    if txp_c.send(row).is_err() {
                        break;
                    }
                }
                tracing::info!("replay bitti: {coin}");
            }
        }));
    }
    drop(txp);

    // 4) TOPLA + YAZ
    let mut all_perf: Vec<Row> = Vec::new();
    while let Ok(p) = rxp.recv() {
        all_perf.push(p);
    }
    for h in handles {
        let _ = h.join();
    }
    tracing::info!("replay bitti: {:.1}s, {} kayıt", t0.elapsed().as_secs_f32(), all_perf.len());

    // AĞIRLIK: strateji başına IS kalite skoru.
    //   kalite = clamp(pf_is-1, 0, 2) * ln(1+işlem/10)   (Kelly yönü: kâr çarpanı)
    //   × (1 - overfit_oranı)   → aşırı uyumlu strateji sıfıra yaklaşır
    let mut agg: HashMap<String, AggAcc> = HashMap::new();
    for (is_p, oos) in &all_perf {
        let e = agg.entry(is_p.strategy.clone()).or_insert(AggAcc::default());
        e.coins += 1;
        e.trades += is_p.trades;
        e.sharpe_sum += is_p.sharpe;
        e.pf_sum += if is_p.profit_factor.is_finite() { is_p.profit_factor } else { 3.0 };
        e.wr_sum += is_p.win_rate;
        e.oos_pf_sum += if oos.profit_factor.is_finite() { oos.profit_factor } else { 3.0 };
        e.oos_sharpe_sum += oos.sharpe;
        if oos.overfit {
            e.overfit_coins += 1;
        }
        if is_p.trades < 10 {
            e.thin_coins += 1;
        }
        e.is_wr += is_p.win_rate;
        e.is_ret += is_p.total_return;
        e.is_mdd += is_p.max_drawdown;
        e.is_bars += is_p.avg_bars;
        e.oos_wr += oos.win_rate;
        e.oos_ret += oos.total_return;
        e.oos_mdd += oos.max_drawdown;
        e.oos_bars += oos.avg_bars;
        e.oos_trades += oos.trades;
    }

    let agg_all = agg.clone();
    let mut scores: Vec<(String, f64, serde_json::Value)> = Vec::new();
    for name in strategies.iter().map(|x| x.name().to_string()) {
        let a = agg.remove(&name).unwrap_or_default();
        let n = a.coins.max(1) as f64;
        let mean_pf = a.pf_sum / n;
        let mean_oos_pf = a.oos_pf_sum / n;
        let overfit_ratio = a.overfit_coins as f64 / n;
        let thin_ratio = a.thin_coins as f64 / n;
        // güven çarpanı: ince örneklem ve OOS çöküşü ağırlığı eritir
        let trust = (1.0 - 0.7 * overfit_ratio) * (1.0 - 0.5 * thin_ratio.max(0.0).min(0.8))
            * if mean_oos_pf >= 0.9 { 1.0 } else { mean_oos_pf / 0.9 };
        let score = (mean_pf - 1.0).clamp(0.0, 2.0) * (1.0 + (a.trades as f64 / 10.0).ln()).min(3.0) * trust;
        let detail = serde_json::json!({
            "is_pf": round3(mean_pf),
            "oos_pf": round3(mean_oos_pf),
            "is_sharpe": round3(a.sharpe_sum / n),
            "oos_sharpe": round3(a.oos_sharpe_sum / n),
            "win_rate": round3(a.wr_sum / n),
            "coins": a.coins,
            "trades": a.trades,
            "overfit_coins": a.overfit_coins,
            "thin_coins": a.thin_coins,
            "overfit_ratio": round3(overfit_ratio),
        });
        scores.push((name, score.max(0.0), detail));
    }
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // yazılacak tablo: ham skor + normalize ağırlık (toplam = 1)
    let total: f64 = scores.iter().map(|x| x.1).sum();
    let mut weights_json = serde_json::Map::new();
    println!("\n{:<28} {:>7} {:>7} {:>8} {:>7} {:>6} {:>7} {:>9}", "STRATEJI", "SKOR", "AĞIRL.", "IS_PF", "OOS_PF", "IS_SH", "OOS_SH", "OVERFIT");
    for (name, score, detail) in &scores {
        let w = if total > 0.0 { score / total } else { 0.0 };
        let mut d = detail.as_object().unwrap().clone();
        d.insert("score".into(), serde_json::json!(round3(*score)));
        d.insert("weight".into(), serde_json::json!(round3(w)));
        // kanonik alanlar (bot/track.rs bu anahtarları okur)
        let of_r = d["overfit_ratio"].as_f64().unwrap_or(0.0);
        d.insert("sharpe".into(), d["oos_sharpe"].clone());
        d.insert("profit_factor".into(), d["oos_pf"].clone());
        d.insert("overfit".into(), serde_json::json!(of_r >= 0.35));
        println!(
            "{:<28} {:>7.3} {:>7.3} {:>8.2} {:>7.2} {:>6.2} {:>7.2} {:>9}",
            name, score, w,
            d["is_pf"], d["oos_pf"], d["is_sharpe"], d["oos_sharpe"],
            d["overfit_coins"]
        );
        weights_json.insert(name.clone(), serde_json::Value::Object(d));
    }

    // 5) CLICKHOUSE: strateji başına IS + OOS satırı (canlı bot buradan okur)
    if let Ok(url) = std::env::var("CLICKHOUSE_URL") {
        if !url.is_empty() {
            let ch = data::storage::ClickHouseStorage::from_env()?;
            let mut written = 0usize;
            for (name, _score, detail) in &scores {
                let a = agg_all.get(name).cloned().unwrap_or_default();
                let n = a.coins.max(1) as f64;
                for (sample, wr, pf, sh, ret, mdd, bars, tr) in [
                    ("IS", a.is_wr / n, a.pf_sum / n, a.sharpe_sum / n, a.is_ret / n, a.is_mdd / n, a.is_bars / n, a.trades),
                    ("OOS", a.oos_wr / n, a.oos_pf_sum / n, a.oos_sharpe_sum / n, a.oos_ret / n, a.oos_mdd / n, a.oos_bars / n, a.oos_trades),
                ] {
                    let row = data::storage::StrategyPerformanceRow {
                        strategy: name.clone(),
                        interval,
                        sample: sample.to_string(),
                        trades: tr,
                        win_rate: round3(wr),
                        profit_factor: round3(pf.min(999.0)),
                        total_return: round3(ret),
                        max_drawdown: round3(mdd),
                        sharpe: round3(sh),
                        avg_bars: round3(bars),
                    };
                    written += ch.save_performance_row(&row).await.unwrap_or(0);
                }
                let _ = detail;
            }
            println!("ClickHouse'a yazıldı: {written} satır → hl_strategy_performance");
        }
    }

    let weights_out = std::env::var("ENSEMBLE_WEIGHTS_OUT")
        .unwrap_or_else(|_| "/root/hyperliquid/config/ensemble_weights.json".into());
    let parent = std::path::Path::new(&weights_out).parent();
    if let Some(par) = parent {
        std::fs::create_dir_all(par)?;
    }
    std::fs::write(&weights_out, serde_json::to_string_pretty(&serde_json::Value::Object(weights_json))?)?;
    println!("KELLY EĞİTİMİ TAMAM ({:.1}s) — ağırlık dosyası: {weights_out}", t0.elapsed().as_secs_f32());
    Ok(())
}
