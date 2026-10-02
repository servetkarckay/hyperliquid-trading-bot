//! BAR-MAJOR REPLAY — bir coin üzerinde TÜM stratejiler tek geçişte:
//! her bar için MarketInput 1 KEZ kurulur (18 strateji paylaşır —
//! run_single'in 18× klon yükü yok), her strateji kendi sanal pozisyonunu
//! yönetir. IS/OOS + overfit tespiti run_single ile aynı kurallarda.

use data::Candle;

use strategy::ensemble::Dir;
use strategy::Strategy;

use crate::backtest::{BtConfig, StrategyPerformance, VPos, ret_of};

/// pencere: stratejilerin en derin ihtiyacı (GARCH ~300 bar)
const WINDOW: usize = 300;

#[derive(Debug, Clone)]
struct StratAcc {
    name: String,
    pos: Option<VPos>,
    trades: u64,
    wins: u64,
    gw: f64,
    gl: f64,
    tr: f64,
    bs: f64,
    equity: Vec<f64>,
    perf_is: Option<StrategyPerformance>,
    perf_oos: Option<StrategyPerformance>,
}

/// Tüm stratejiler × tek coin; IS ve OOS ayrı ölçülür; overfit işaretlenir.
pub fn run_multi(
    strategies: &[Box<dyn Strategy>],
    coin: &str,
    candles: &[Candle],
    returns_at: &dyn Fn(usize) -> Vec<(String, f64)>,
    cfg: &BtConfig,
) -> Vec<(StrategyPerformance, StrategyPerformance)> {
    let total = candles.len();
    let is_bar = (total as f64 * cfg.is_split) as usize;
    let cost = (cfg.taker_fee_bps + cfg.slippage_bps) / 10_000.0;
    let closes: Vec<f64> = candles.iter().map(|c| c.c).collect();

    let mut accs: Vec<StratAcc> = strategies
        .iter()
        .map(|s| StratAcc {
            name: s.name().to_string(),
            pos: None,
            trades: 0,
            wins: 0,
            gw: 0.0,
            gl: 0.0,
            tr: 0.0,
            bs: 0.0,
            equity: Vec::new(),
            perf_is: None,
            perf_oos: None,
        })
        .collect();

    for (sample, from, to) in [("IS", 0usize, is_bar), ("OOS", is_bar, total)] {
        if to - from < 30 {
            continue;
        }
        for a in accs.iter_mut() {
            a.pos = None;
            a.trades = 0;
            a.wins = 0;
            a.gw = 0.0;
            a.gl = 0.0;
            a.tr = 0.0;
            a.bs = 0.0;
            a.equity.clear();
        }

        for i in from..to {
            let price = closes[i];
            let w_start = i.saturating_sub(WINDOW);
            let input = strategy::MarketInput {
                coin: coin.to_string(),
                candles: candles[w_start..=i].to_vec(),
                universe_returns: returns_at(i),
                equity: 100.0,
                ..Default::default()
            };
            // input'ı 1 kez kur → 18 strateji paylaşır
            for (si, strat) in strategies.iter().enumerate() {
                let sig = strat.evaluate(&input);
                let a = &mut accs[si];
                let exit_fill = candles.get(i + 1).map(|c| c.o).unwrap_or(price);

                let (must_exit, against) = match &a.pos {
                    Some(p) => {
                        let against = match (&sig, p.side) {
                            (strategy::Signal::Short { strength, .. }, Dir::Long) => *strength > 0.35,
                            (strategy::Signal::Long { strength, .. }, Dir::Short) => *strength > 0.35,
                            _ => false,
                        };
                        (
                            matches!(&sig, strategy::Signal::Exit { .. })
                                || against
                                || i - p.entry_i >= cfg.max_hold_bars,
                            against,
                        )
                    }
                    None => (false, false),
                };
                if must_exit {
                    if let Some(p) = a.pos.take() {
                        let ret = ret_of(p.side, p.entry, exit_fill) - cost;
                        let usd = ret * cfg.notional_usd;
                        if usd >= 0.0 { a.gw += usd; a.wins += 1; } else { a.gl += -usd; }
                        a.tr += ret;
                        a.bs += (i - p.entry_i) as f64;
                        a.trades += 1;
                    }
                }
                if a.pos.is_none() {
                    let dir = match &sig {
                        strategy::Signal::Long { strength, .. } if *strength >= 0.4 => Some(Dir::Long),
                        strategy::Signal::Short { strength, .. } if *strength >= 0.4 => Some(Dir::Short),
                        _ => None,
                    };
                    if let Some(side) = dir {
                        let fill_px = candles.get(i + 1).map(|c| c.o).unwrap_or(price);
                        a.pos = Some(VPos { side, entry: fill_px, entry_i: i });
                    }
                }
                let mark = match &a.pos {
                    Some(p) => ret_of(p.side, p.entry, price) - cost,
                    None => 0.0,
                };
                let prev = *a.equity.last().unwrap_or(&1.0);
                a.equity.push(prev * (1.0 + mark));
            }
        }
        // kalan pozisyonları kapat (OOS sonu)
        for a in accs.iter_mut() {
            if let Some(p) = a.pos.take() {
                let exit_fill = closes[(to - 1).min(total - 1)];
                let ret = ret_of(p.side, p.entry, exit_fill) - cost;
                let usd = ret * cfg.notional_usd;
                if usd >= 0.0 { a.gw += usd; a.wins += 1; } else { a.gl += -usd; }
                a.tr += ret;
                a.bs += (to - 1 - p.entry_i) as f64;
                a.trades += 1;
                let prev = *a.equity.last().unwrap_or(&1.0);
                a.equity.push(prev * (1.0 + ret));
            }
        }

        let mut sample_perfs: Vec<StrategyPerformance> = Vec::with_capacity(accs.len());
        for a in accs.iter() {
            sample_perfs.push(crate::backtest::assemble(
                &a.name,
                coin,
                sample,
                a.trades,
                a.wins,
                a.gw,
                a.gl,
                a.tr,
                a.bs,
                &a.equity,
            ));
        }
        for (a, p) in accs.iter_mut().zip(sample_perfs) {
            if sample == "IS" {
                a.perf_is = Some(p);
            } else {
                a.perf_oos = Some(p);
            }
        }
    }

    // SONUÇ: (IS, OOS) çifti; overfit işareti ikisine de yazılır
    accs.iter()
        .map(|a| {
            let mut is_p = a.perf_is.clone().unwrap_or_default();
            let mut oos = a.perf_oos.clone().unwrap_or_default();
            let overfit = is_p.trades >= 10
                && is_p.profit_factor >= 1.2
                && (oos.trades < 5 || (oos.profit_factor < 0.9 && oos.trades >= 5));
            if overfit {
                oos.verdict = "OVERFIT".into();
            }
            is_p.strategy = a.name.clone();
            is_p.coin = coin.to_string();
            is_p.overfit = overfit;
            oos.strategy = a.name.clone();
            oos.coin = coin.to_string();
            oos.overfit = overfit;
            (is_p, oos)
        })
        .collect()
}
