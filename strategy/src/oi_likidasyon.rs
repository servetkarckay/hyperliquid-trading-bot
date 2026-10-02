//! OI/LİKİDASYON — ΔOI × ΔP matrisi karar tablosu.
//!
//! HL'de piyasa geneli likidasyon feed'i yok; OI değişiminden ÇIKARIMSAL:
//!   OI↑ + P↑ = yeni long girişi -> Long (trend devam)
//!   OI↑ + P↓ = yeni short girişi -> Short
//!   OI↓ + P↓ = long likidasyon kaskadı -> kontra Long (dip)
//!   OI↓ + P↑ = short squeeze -> kontra Short (zirve)
//! Güç: |ΔOI|/son fiyat göreliliği [0.2-0.8]; kontra yönler %30 zayıf.
//! GEREKLİ VERİ: OI deltası (MarketCtx OI serisi) + >=13 mum.
use crate::{closes, MarketInput, Signal, Strategy};

pub struct OiLikidasyon;

impl Strategy for OiLikidasyon {
    fn name(&self) -> &'static str {
        "OI Likidasyon"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let Some(d_oi) = input.oi_delta else {
            return Signal::Skip { reason: "ΔOI yok (ctx serisi kısa)".into() };
        };
        let c = closes(&input.candles);
        if c.len() < 13 {
            return Signal::Skip { reason: ">=13 mum gerekli".into() };
        }
        let d_px = c[c.len() - 1] - c[c.len() - 13];
        let oi_up = d_oi > 0.0;
        let px_up = d_px > 0.0;

        // görece büyüklük: |ΔOI| son fiyatın kaç bini — küçük değişim sinyal değil
        let strength = ((d_oi.abs() / c[c.len() - 1].max(1.0)) * 1e4).min(0.8).max(0.2);

        match (oi_up, px_up) {
            (true, true) => Signal::Long { strength, note: "OI↑ + P↑: yeni long girişi, trend devam".into() },
            (true, false) => Signal::Short { strength, note: "OI↑ + P↓: yeni short girişi, düşüş devam".into() },
            (false, false) => Signal::Long { strength: strength * 0.7, note: "OI↓ + P↓: long likidasyon kaskadı — kontra dip".into() },
            (false, true) => Signal::Short { strength: strength * 0.7, note: "OI↓ + P↑: short squeeze — kontra zirve".into() },
        }
    }
}
