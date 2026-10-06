//! OI LİKİDASYON — ΔOI × ΔP kadrans matrisi.
//!
//! 1. OI↑ + P↑ → Long (trend devam); OI↑ + P↓ → Short; OI↓ + P↓ →
//! kontra Long (likidasyon kaskadı, ×0.7); OI↓ + P↑ → kontra Short
//! (squeeze, ×0.7).
//! 2. Güç: |ΔOI|/px göreliliği [0.2–0.8]; ΔOI yok veya <13 mum → Skip
//! (HL'de ayrı likidasyon feed'i yok — OI'dan çıkarımsal; sahte kadrans yok).
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

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

 /// A — OI↑ + P↑ → Long (trend devam).
    #[test]
    fn oi_up_px_up_long() {
        let mut candles = H::linear(20, 100.0, 0.5); // fiyat yükseliyor
        let input = MarketInput { coin: "T".into(), candles, oi_delta: Some(2.0), ..Default::default() };
        match OiLikidasyon.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!(strength >= 0.2 && strength <= 0.8);
                assert!(note.contains("trend devam"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

 /// C — 4 kadrans simetrisi: OI↑+P↓ → Short; OI↓+P↓ → kontra Long (%30
 /// zayıf); OI↓+P↑ → kontra Short (%30 zayıf).
    #[test]
    fn oi_dort_kadrans_yonleri() {
        let mk = |candles: Vec<data::Candle>, d: f64| MarketInput { coin: "T".into(), candles, oi_delta: Some(d), ..Default::default() };
 // OI↑ + P↓ → Short
        let down = H::linear(20, 110.0, -0.5);
        assert!(matches!(OiLikidasyon.evaluate(&mk(down, 2.0)), Signal::Short { strength, .. } if strength >= 0.2));
 // OI↓ + P↓ → kontra Long (×0.7)
        let down2 = H::linear(20, 110.0, -0.5);
        let trend_long = match OiLikidasyon.evaluate(&mk(H::linear(20, 100.0, 0.5), 2.0)) {
            Signal::Long { strength, .. } => strength,
            other => panic!("{other:?}"),
        };
        match OiLikidasyon.evaluate(&mk(down2, -2.0)) {
            Signal::Long { strength, note } => {
                assert!((strength - trend_long * 0.7).abs() < 1e-9, "kontra %30 zayıf: {strength} vs {trend_long}");
                assert!(note.contains("kontra"), "{note}");
            }
            other => panic!("{other:?}"),
        }
 // OI↓ + P↑ → kontra Short (×0.7)
        let up = H::linear(20, 100.0, 0.5);
        assert!(matches!(OiLikidasyon.evaluate(&mk(up, -2.0)), Signal::Short { strength, .. } if strength >= 0.14));
    }

 /// D — ΔOI yoksa Skip (sahte kadrans yasağı).
    #[test]
    fn oi_veri_yoksa_skip() {
        let input = MarketInput { coin: "T".into(), candles: H::flat(20, 100.0), oi_delta: None, ..Default::default() };
        assert!(matches!(OiLikidasyon.evaluate(&input), Signal::Skip { .. }));
        let kisa = MarketInput { coin: "T".into(), candles: H::flat(10, 100.0), oi_delta: Some(1.0), ..Default::default() };
        assert!(matches!(OiLikidasyon.evaluate(&kisa), Signal::Skip { .. }));
    }
}
