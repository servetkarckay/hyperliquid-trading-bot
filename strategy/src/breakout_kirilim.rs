//! BREAKOUT — sıkışma ön koşuluyla kırılım OLAY dedektörü.
//!
//! SÜREÇ:
//!   1. Sıkışma: Bollinger(20,2) genişlik yüzdeliği < %35 (son 100 bar)
//!   2. Tetik: fiyat son 30 barın zirvesine/ducini geçmesi
//!   3. Onay: taker akışı + OI değişimi
//!   4. Kırılım = OLAY, yön tahmini değil — iki tarafta simetrik
//! Sinyal: sıkışma yoksa Hold (sıkışma olmadan girilmez), tetikte
//! Long/Short [0.75].
//! GEREKLİ VERİ: >=120 mum + canlı taker/OI.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct BreakoutKirilim;

impl Strategy for BreakoutKirilim {
    fn name(&self) -> &'static str {
        "Breakout"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 120 {
            return Signal::Skip { reason: ">=120 mum gerekli".into() };
        }
        let price = c[c.len() - 1];

        // BB genişliği serisi ve yüzdelik
        let bb = indicators::bollinger(&c, 20, 2.0);
        let width: Vec<f64> = bb
            .iter()
            .enumerate()
            .filter_map(|(i, (m, up, lo))| match (m, up, lo) {
                (Some(m), Some(u), Some(l)) if *m > 0.0 => Some((u - l) / m),
                _ => {
                    let _ = i;
                    None
                }
            })
            .collect();
        if width.len() < 100 {
            return Signal::Skip { reason: "BB genişlik serisi kısa".into() };
        }
        let cur_width = width[width.len() - 1];
        let hist = &width[width.len() - 100..];
        let pct = hist.iter().filter(|w| **w <= cur_width).count() as f64 / hist.len() as f64;

        // sıkışma ön koşulu: son genişlik ilk %20'lik dilimdeydi (son 5 bar öncesi)
        let was_squeezed = pct < 0.35;

        // kırılım tetiği: fiyat son 30 barın zirvesini geçti mi
        let hh30 = c[c.len() - 31..c.len() - 1].iter().cloned().fold(f64::MIN, f64::max);
        let ll30 = c[c.len() - 31..c.len() - 1].iter().cloned().fold(f64::MAX, f64::min);
        let breakout_up = price > hh30;
        let breakout_dn = price < ll30;

        // onaylar: taker + OI
        let taker_buy = input.taker_ratio.unwrap_or(0.5) > 0.55;
        let taker_sell = input.taker_ratio.unwrap_or(0.5) < 0.45;
        let oi_up = input.oi_delta.unwrap_or(0.0) > 0.0;
        let oi_dn = input.oi_delta.unwrap_or(0.0) < 0.0;

        if was_squeezed && breakout_up && taker_buy {
            return Signal::Long {
                strength: 0.75,
                note: format!("sıkışma (%{pct:.0} genişlik) sonrası yukarı kırılım + taker alıcı{} — OI{}", 
                    if oi_up { " + OI↑" } else { "" }, if oi_up { "↑" } else { "~" }),
            };
        }
        if was_squeezed && breakout_dn && taker_sell {
            return Signal::Short {
                strength: 0.75,
                note: format!("sıkışma sonrası aşağı kırılım + taker satıcı{}",
                    if oi_dn { " + OI↓" } else { "" }),
            };
        }
        if was_squeezed {
            Signal::Hold { note: format!("sıkışma var (%{pct:.0}), kırılım bekleniyor (HH30={hh30:.0} LL30={ll30:.0})") }
        } else {
            Signal::Hold { note: format!("sıkışma yok (genişlik %{:.0} yüzdelik)", pct * 100.0) }
        }
    }
}
