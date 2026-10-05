//! VOLATİLİTE ARBİTRAJI — VRP (vol risk premium) katmanı.
//!
//! 1. IV feed bağlıyken GERÇEK VRP = IV − RV30: VRP>0 → Short (vol
//!    satışı lehine), VRP<0 → Long; güç (|VRP|/(IV·0.5)).min(0.7).
//! 2. IV yok: yerel RV terim yapısı proxy (10-bar/100-bar) — yalnız
//!    Hold NOTU, yön üretmez; **SAHTE IV ÜRETİLMEZ**.
//! 3. Durum: IV'süz → DATA-MISSING (raporda ayrışır); IV'li → ACTIVE.
use crate::{closes, MarketInput, Signal, StratStatus, Strategy};
use math::indicators;

pub struct VolatiliteArbitraj;

impl Strategy for VolatiliteArbitraj {
    fn name(&self) -> &'static str {
        "Vol Arbitraj (VRP)"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        // harici IV varsa gerçek VRP
        if let Some(iv) = input.iv {
            let c = closes(&input.candles);
            if c.len() < 60 {
                return Signal::Skip { reason: ">=60 mum gerekli".into() };
            }
            let r = indicators::log_returns(&c);
            let w = &r[r.len().saturating_sub(30)..];
            let m = w.iter().sum::<f64>() / w.len() as f64;
            let rv = (w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (w.len() - 1) as f64).sqrt()
                * (365.0 * 24.0f64).sqrt();
            let vrp = iv - rv;
            if vrp > 0.0 {
                return Signal::Short { strength: (vrp / (iv * 0.5)).min(0.7), note: format!("VRP %{:.1}: IV %{:.0} > RV %{:.0} — vol satışı lehine (çarpan katmanı)", vrp * 100.0, iv * 100.0, rv * 100.0) };
            }
            return Signal::Long { strength: ((-vrp) / (iv * 0.5)).min(0.7), note: format!("VRP negatif: IV < RV — vol alışı lehine") };
        }

        // IV yoksa yerel terim yapısı proxy (RV kısa/uzun oranı)
        let c = closes(&input.candles);
        if c.len() < 120 {
            return Signal::Skip { reason: ">=120 mum gerekli (RV terim yapısı)".into() };
        }
        let r = indicators::log_returns(&c);
        let rv_short = { let w = &r[r.len().saturating_sub(10)..]; (w.iter().map(|x| x * x).sum::<f64>() / 10.0).sqrt() };
        let rv_long = { let w = &r[r.len().saturating_sub(100)..]; (w.iter().map(|x| x * x).sum::<f64>() / 100.0).sqrt() };
        if rv_long <= 0.0 {
            return Signal::Skip { reason: "RV uzun dönem sıfır".into() };
        }
        let ratio = rv_short / rv_long;
        if ratio < 0.6 {
            Signal::Hold { note: format!("RV kısa/uzun={ratio:.2} — vol sıkışması, genişleme riski (IV feed'i bağlanınca gerçek VRP)") }
        } else if ratio > 1.6 {
            Signal::Hold { note: format!("RV kısa/uzun={ratio:.2} — vol gerileme beklentisi") }
        } else {
            Signal::Hold { note: format!("RV terim yapısı nötr: {ratio:.2}") }
        }
    }

    /// IV feed bağlı değilken GERÇEK VRP üretilemez → `DATA-MISSING`.
    /// (SAHTE IV ÜRETMEZ: yerel RV proxy yalnız bilgi notudur, skor üretmez.)
    fn status(&self, input: &MarketInput) -> StratStatus {
        if input.iv.is_none() {
            StratStatus::DataMissing
        } else {
            StratStatus::Active
        }
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

    /// Salınımlı seri (RV > 0 gerçekçi)
    fn osc(n: usize, a: f64) -> Vec<data::Candle> {
        (0..n)
            .map(|i| {
                let px = 100.0 + a * (if i % 2 == 0 { 1.0 } else { -1.0 });
                H::candle(i as i64 * 3_600_000, px, px + 0.05, px - 0.05, px, 100.0)
            })
            .collect()
    }

    /// A — VRP: IV > RV30 → vol satışı lehine → Short; IV < RV → Long.
    #[test]
    fn vrp_iv_varken_yon_dogru() {
        // RV ~%70 yıllık (a=0.7, 1h barlar), IV %65 → VRP<0 → Long
        let iv = MarketInput { coin: "T".into(), candles: osc(70, 0.7), iv: Some(0.65), ..Default::default() };
        match VolatiliteArbitraj.evaluate(&iv) {
            Signal::Long { strength, note } => {
                assert!(strength > 0.0 && strength <= 0.7);
                assert!(note.contains("VRP"), "{note}");
            }
            other => panic!("IV< RV → Long beklenirdi: {other:?}"),
        }
        // IV %300 > RV %70 → VRP>0 → Short (vol satışı lehine)
        let iv2 = MarketInput { coin: "T".into(), candles: osc(70, 0.7), iv: Some(3.0), ..Default::default() };
        match VolatiliteArbitraj.evaluate(&iv2) {
            Signal::Short { strength, note } => {
                assert!(strength > 0.0 && strength <= 0.7);
                assert!(note.contains("vol satışı lehine"), "{note}");
            }
            other => panic!("VRP>0 → Short beklenirdi: {other:?}"),
        }
    }

    /// D — IV yok → yönlü VRP ÜRETİLMEZ (yerel proxy yalnız Hold notu;
    /// SAHTE IV ÜRETİLMEZ) + durum DATA-MISSING.
    #[test]
    fn vrp_iv_yokken_sahte_vrp_uretmez() {
        let c = osc(130, 0.7);
        let input = MarketInput { coin: "T".into(), candles: c.clone(), iv: None, ..Default::default() };
        match VolatiliteArbitraj.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("RV") || note.contains("proxy") || note.contains("vol"), "{note}"),
            other => panic!("IV'sız proxy yönsüz olmalı: {other:?}"),
        }
        assert_eq!(VolatiliteArbitraj.status(&input), StratStatus::DataMissing);
        let with_iv = MarketInput { coin: "T".into(), candles: c, iv: Some(0.5), ..Default::default() };
        assert_eq!(VolatiliteArbitraj.status(&with_iv), StratStatus::Active);
    }
}
