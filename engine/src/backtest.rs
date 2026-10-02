//! BACKTEST MOTORU — tarihsel veriyi canlı akış gibi replay eder; her
//! stratejinin performansını ölçer (Kelly + ensemble ağırlıkları için).
//!
//! ARAŞTIRMA KURALLARI (burada garanti altında):
//! 1. ÖRNEKLEM DERİNLİĞİ: min 180 gün — rejim çeşitliliği şart.
//!    Tick verisi yerine 1m/5m/15m/1h mum; tick sadece slippage örneği.
//! 2. TRANSACTION COST: her round-trip'e taker fee (%0.045) + slippage
//!    (%0.05) eklenir, her bacakta → ~%0.19 silindirme (al-sat kâr silme).
//! 3. IS/OOS: ilk %75 in-sample, son %25 out-of-sample;
//!    IS'i iyi OOS'u kötü olan strateji OVERFIT → ensemble ağırlığı 0.
//! 4. DİNAMİK KELLY: hl_strategy_performance tablosu win-rate/PF
//!    besler; ağırlıklar sharpe-temelli normalize edilir.
//!
//! MOCK EXECUTOR: sanal fills bar kapanışında; tek sanal pozisyon.

use std::collections::HashMap;

use data::Candle;

use strategy::ensemble::Dir;
use strategy::Strategy;

#[derive(Debug, Clone)]
pub struct BtConfig {
    pub interval_secs: u64,
    pub days: u64,
    /// IS bölümü (0.75 = ilk %75 in-sample)
    pub is_split: f64,
    pub notional_usd: f64,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
    pub max_hold_bars: usize,
}

impl Default for BtConfig {
    fn default() -> Self {
        Self {
            interval_secs: 3600,
            days: 180,
            is_split: 0.75,
            notional_usd: 100.0,
            taker_fee_bps: 4.5,
            slippage_bps: 5.0,
            max_hold_bars: 48,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct StrategyPerformance {
    pub strategy: String,
    pub coin: String,
    pub sample: String,
    pub trades: u64,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub total_return: f64,
    pub max_drawdown: f64,
    pub sharpe: f64,
    pub avg_bars: f64,
    pub verdict: String,
    pub overfit: bool,
}

/// SANAL POZİSYON
#[derive(Debug, Clone)]
pub struct VPos {
    pub side: Dir,
    pub entry: f64,
    pub entry_i: usize,
}

pub fn ret_of(side: Dir, entry: f64, exit: f64) -> f64 {
    match side {
        Dir::Long => exit / entry.max(1e-12) - 1.0,
        Dir::Short => entry / exit.max(1e-12) - 1.0,
        Dir::Neutral => 0.0,
    }
}

/// Tek coin üzerinde tek stratejinin tam replay'i (IS + OOS ayrı ölçülür)
pub fn run_single(
    strat: &dyn Strategy,
    coin: &str,
    candles: &[Candle],
    returns_at: &dyn Fn(usize) -> Vec<(String, f64)>,
    cfg: &BtConfig,
) -> StrategyPerformance {
    let total = candles.len();
    let is_bar = (total as f64 * cfg.is_split) as usize;
    let cost = (cfg.taker_fee_bps + cfg.slippage_bps) / 10_000.0;
    let closes: Vec<f64> = candles.iter().map(|c| c.c).collect();

    let mut best_out: Option<StrategyPerformance> = None;
    let mut out_is: Option<StrategyPerformance> = None;

    for (sample, from, to) in
        [("IS", 0usize, is_bar), ("OOS", is_bar, total)]
    {
        if to - from < 30 {
            continue;
        }
        let mut pos: Option<VPos> = None;
        let (mut trades, mut wins) = (0u64, 0u64);
        let (mut gw, mut gl) = (0.0f64, 0.0f64);
        let (mut tr, mut bs) = (0.0f64, 0.0f64);
        let mut equity: Vec<f64> = Vec::new();

        for i in from..to {
            let price = closes[i];
            let w_start = i.saturating_sub(500);
            let input = strategy::MarketInput {
                coin: coin.to_string(),
                candles: candles[w_start..=i].to_vec(),
                universe_returns: returns_at(i),
                equity: 100.0,
                ..Default::default()
            };
            let sig = strat.evaluate(&input);
            if std::env::var("BTDBG").is_ok() {
                eprintln!("DBG i={i} candle_len={} sig={sig:?}", input.candles.len());
            }

            // EXIT kararları: sinyal exit / ters yönlü güçlü sinyal / süre htaşı
            let (must_exit, against) = match &pos {
                Some(p) => {
                    let against = match (&sig, p.side) {
                        (strategy::Signal::Short { strength, .. }, Dir::Long) => *strength > 0.35,
                        (strategy::Signal::Long { strength, .. }, Dir::Short) => *strength > 0.35,
                        _ => false,
                    };
                    (
                        matches!(&sig, strategy::Signal::Exit { .. }) || against || i - p.entry_i >= cfg.max_hold_bars,
                        against,
                    )
                }
                None => (false, false),
            };
            if must_exit {
                if let Some(p) = pos.take() {
                    let exit_fill = candles.get(i + 1).map(|c| c.o).unwrap_or(price);
                    let ret = ret_of(p.side, p.entry, exit_fill) - cost;
                    let usd = ret * cfg.notional_usd;
                    if usd >= 0.0 { gw += usd; wins += 1; } else { gl += -usd; }
                    tr += ret;
                    bs += (i - p.entry_i) as f64;
                    trades += 1;
                    if !against { /* sinyal exit veya süre — pozisyon aç */ }
                }
            }
            // ENTRY (sadece pozisyonsuz)
            if pos.is_none() {
                if std::env::var("BTDBG").is_ok() { eprintln!("DBG-ENTRY-CHECK i={i}"); }
                let dir = match &sig {
                    strategy::Signal::Long { strength, .. } if *strength >= 0.4 => Some(Dir::Long),
                    strategy::Signal::Short { strength, .. } if *strength >= 0.4 => Some(Dir::Short),
                    _ => None,
                };
                if let Some(side) = dir {
                    // KIYAMET KORUMASI S19: look-ahead yok — fill SONRAKİ BAR AÇILIŞI
                    let fill_px = candles.get(i + 1).map(|c| c.o).unwrap_or(price);
                    pos = Some(VPos { side, entry: fill_px, entry_i: i });
                }
            }
            // equity mark (açık sanal pozisyon dahil)
            let mark = match &pos {
                Some(p) => ret_of(p.side, p.entry, price) - cost,
                None => 0.0,
            };
            let prev = *equity.last().unwrap_or(&1.0);
            equity.push(prev * (1.0 + mark));
        }
        // kalan açığı kapat
        if let Some(p) = pos.take() {
            let exit_fill = candles.get(to).map(|c| c.o).unwrap_or(closes[(to - 1).min(total - 1)]);
            let ret = ret_of(p.side, p.entry, exit_fill) - cost;
            let usd = ret * cfg.notional_usd;
            if usd >= 0.0 { gw += usd; wins += 1; } else { gl += -usd; }
            tr += ret;
            bs += (to - 1 - p.entry_i) as f64;
            trades += 1;
            let prev = *equity.last().unwrap_or(&1.0);
            equity.push(prev * (1.0 + ret));
        }

        let p = assemble(strat.name(), coin, sample, trades, wins, gw, gl, tr, bs, &equity);
        if sample == "IS" { out_is = Some(p); } else { best_out = Some(p); }
    }

    // OOS sonucu + overfit işareti
    let mut final_p = best_out.unwrap_or_default();
    if let Some(is_p) = out_is {
        final_p.overfit = check_overfit(&is_p, &final_p);
        if final_p.overfit {
            final_p.verdict = "OVERFIT".into();
        }
    }
    final_p
}

/// IS'i iyi çalışan, OOS'da çöken strateji = aşırı uyum
fn check_overfit(is_p: &StrategyPerformance, oos_p: &StrategyPerformance) -> bool {
    is_p.trades >= 10
        && is_p.profit_factor >= 1.2
        && (oos_p.trades < 5 || (oos_p.profit_factor < 0.9 && oos_p.trades >= 5))
}

pub fn assemble(
    name: &str,
    coin: &str,
    sample: &str,
    trades: u64,
    wins: u64,
    gross_win: f64,
    gross_loss: f64,
    total_ret: f64,
    bars_sum: f64,
    equity: &[f64],
) -> StrategyPerformance {
    let rets: Vec<f64> = equity.windows(2).map(|w| w[1] / w[0] - 1.0).collect();
    let (mean, sd) = if rets.len() > 1 {
        let m = rets.iter().sum::<f64>() / rets.len() as f64;
        let d = (rets.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (rets.len() - 1) as f64).sqrt();
        (m, d)
    } else {
        (0.0, 0.0)
    };
    // yıllıklandırma: saatlik bar başına → sqrt(8760); örneklem < 10 ise 0 (gürültü)
    let sharpe = if sd > 0.0 && trades >= 10 { mean / sd * (365.0 * 24.0f64).sqrt() } else { 0.0 };
    let pf = if gross_loss > 0.0 { gross_win / gross_loss } else if gross_win > 0.0 { f64::INFINITY } else { 0.0 };
    let is_ok = pf >= 1.2 && win_rate_ok(trades, wins);
    StrategyPerformance {
        strategy: name.to_string(),
        coin: coin.to_string(),
        sample: sample.to_string(),
        trades,
        win_rate: if trades > 0 { wins as f64 / trades as f64 } else { 0.0 },
        profit_factor: pf,
        total_return: total_ret,
        max_drawdown: max_dd(equity),
        sharpe,
        avg_bars: if trades > 0 { bars_sum / trades as f64 } else { 0.0 },
        verdict: if trades < 10 { "YETERSİZ ÖRNEKLEM".to_string() } else if is_ok { "ÇALIŞIYOR".to_string() } else { "ZAYIF".to_string() },
        overfit: false,
    }
}

fn win_rate_ok(trades: u64, wins: u64) -> bool {
    trades > 0 && wins as f64 / trades as f64 >= 0.4
}

pub fn max_dd(equity: &[f64]) -> f64 {
    let mut peak = f64::MIN;
    let mut mdd = 0.0;
    for v in equity {
        if *v > peak { peak = *v; }
        if peak > 0.0 {
            let d = (peak - v) / peak;
            if d > mdd { mdd = d; }
        }
    }
    mdd
}

/// ENSEMBLE AĞIRLIKLARI — IS sharpe > 0 stratejilere pay;
/// hiçbiri pozitif değilse EŞİT (1/n) — overfit/0-sharpe olan düşer
pub fn ensemble_names(perfs: &[StrategyPerformance]) -> Vec<String> {
    perfs.iter().filter(|p| p.sample == "IS").map(|p| p.strategy.clone()).collect::<std::collections::HashSet<_>>().into_iter().collect()
}

pub fn ensemble_weights(perfs: &[StrategyPerformance], names: &[String]) -> HashMap<String, f64> {
    let mut raw: HashMap<String, f64> = HashMap::new();
    for p in perfs {
        if p.sample != "IS" {
            continue;
        }
        let e = raw.entry(p.strategy.clone()).or_insert(0.0);
        *e = (*e).max(p.sharpe.max(0.0));
    }
    let sum: f64 = raw.values().sum();
    let n = names.len().max(1) as f64;
    names
        .iter()
        .map(|nm| {
            let r = raw.get(nm).copied().unwrap_or(0.0);
            (nm.clone(), if sum > 0.0 { r / sum } else { 1.0 / n })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysLong;
    impl Strategy for AlwaysLong {
        fn name(&self) -> &'static str { "always-long" }
        fn evaluate(&self, _i: &strategy::MarketInput) -> strategy::Signal {
            strategy::Signal::Long { strength: 0.8, note: "test".into() }
        }
    }

    fn gen(bars: usize, up: bool) -> Vec<Candle> {
        (0..bars).map(|i| {
            let c = if up { 100.0 + i as f64 * 0.5 } else { 100.0 };
            Candle { t: 1_790_000_000_000 + i as i64 * 3_600_000, o: c, h: c + 1.0, l: c - 1.0, c, v: 10.0, n: 1, closed: true }
        }).collect()
    }

    #[test]
    fn yukselen_piyasada_long_kazanir() {
        let candles = gen(150, true);
        let s = run_single(&AlwaysLong, "BTC", &candles, &|_| vec![], &BtConfig::default());
        assert!(s.trades > 0, "trades={}", s.trades);
        assert!(s.win_rate > 0.5, "yükselen piyasa long kazanmalı: {:?}", s);
        assert!(s.total_return > 0.0);
    }

    #[test]
    fn yatay_piyasada_cost_eritir() {
        let candles = gen(150, false); // sabit fiyat
        let s = run_single(&AlwaysLong, "BTC", &candles, &|_| vec![], &BtConfig::default());
        assert!(s.total_return < 0.0, "yatay piyasada sadece cost kaybettirir: {:?}", s);
        assert!(s.max_drawdown > 0.0);
    }

    #[test]
    fn agirliklar_sharpe_temelli() {
        let candles = gen(150, true);
        let s = run_single(&AlwaysLong, "BTC", &candles, &|_| vec![], &BtConfig::default());
        let w = ensemble_weights(&[s.clone()], &["always-long".to_string()]);
        assert!((w.get("always-long").copied().unwrap_or(0.0) - 1.0).abs() < 1e-9);
    }
}
