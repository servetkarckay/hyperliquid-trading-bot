//! WYCKOFF — arz-talep kuramı + efor/sonuç divergence.
//!
//! AMAÇ: trendin hacimle ONAYLANIP onaylanmadığını ölçmek.
//! HESAPLAMA:
//! 1. Trend yönü: EMA20 vs EMA50 (yapı yönü)
//! 2. Efor: son barın hacmi / 20-bar ortalama hacim
//!    (>1.8 burst = birikim/dağıtım patlaması, <0.7 dry-up = kuruma)
//! 3. Sonuç: son 20 barın zirve-dip aralığında konum (<0.05 dip, >0.95 zirve)
//! 4. Karar:
//!    - yukarı trend + hacim onayı (efor>1.2) -> Long [0.4-0.9]
//!    - düşüş trend + hacim onayı -> Short
//!    - zirvede hacim kurusu -> dağıtım, kontra Short [0.3]
//!    - diple hacim patlaması -> birikim/spring, kontra Long [0.5]
//! GEREKLİ VERİ: >=60 mum + hacim.
use crate::{closes, last, volumes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct Wyckoff;

const VOL_AVG: usize = 20;

impl Strategy for Wyckoff {
    fn name(&self) -> &'static str {
        "Wyckoff"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 60 {
            return Signal::Skip { reason: ">=60 mum gerekli".into() };
        }
        let vols = volumes(&input.candles);
        let e20 = last(&indicators::ema(&c, 20)).unwrap();
        let e50 = last(&indicators::ema(&c, 50)).unwrap();
        let price = c[c.len() - 1];

        // trend yönü
        let trend = if e20 > e50 { 1.0 } else { -1.0 };

        // efor (hacim) / sonuç (fiyat): son barın hacmi 20-bar ortalamanın kaç katı
        let v_avg: f64 = vols[vols.len() - VOL_AVG..].iter().sum::<f64>() / VOL_AVG as f64;
        let v_last = vols[vols.len() - 1];
        let effort = v_last / v_avg.max(1e-12);

        // 20 barlık aralık pozisyonu (0=alt, 1=üst)
        let hh = highs_of(&input.candles);
        let ll = lows_of(&input.candles);
        let range = (hh - ll).abs().max(1e-12);
        let pos = (price - ll) / range;

        // yeni zirve + hacim kurusu = dağıtım (short); yeni dip + hacim patlaması = birikim (long)
        let at_high = pos > 0.95;
        let at_low = pos < 0.05;
        let dry_up = effort < 0.7;
        let burst = effort > 1.8;

        if trend > 0.0 && !at_high && effort > 1.2 {
            return Signal::Long {
                strength: (0.4 + effort.min(3.0) * 0.1).min(0.9),
                note: format!("trend yukarı (E20>E50), hacim onaylı ({effort:.1}x), aralık konumu {pos:.2}"),
            };
        }
        if trend < 0.0 && !at_low && effort > 1.2 {
            return Signal::Short {
                strength: (0.4 + effort.min(3.0) * 0.1).min(0.9),
                note: format!("trend aşağı, hacim onaylı"),
            };
        }
        if at_high && dry_up {
            return Signal::Short { strength: 0.3, note: "yeni zirve + hacim kurusu (dağıtım)".into() };
        }
        if at_low && burst {
            return Signal::Long { strength: 0.5, note: "yeni dip + hacim patlaması (birikim/spring)".into() };
        }
        Signal::Hold { note: format!("karışık: trend={trend:+.0} effort={effort:.1} pos={pos:.2}") }
    }
}

fn highs_of(candles: &[data::Candle]) -> f64 {
    candles[candles.len() - 20..].iter().map(|c| c.h).fold(f64::MIN, f64::max)
}
fn lows_of(candles: &[data::Candle]) -> f64 {
    candles[candles.len() - 20..].iter().map(|c| c.l).fold(f64::MAX, f64::min)
}
