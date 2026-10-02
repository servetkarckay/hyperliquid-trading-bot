//! VOLATİLİTE ARBİTRAJI — VRP (vol risk premium) çarpan katmanı.
//!
//! GERÇEK VRP (IV feed bağlı): VRP = IV - RV30;
//!   VRP>0 -> vol satışı lehine; VRP<0 -> vol alışı lehine
//! YEREL PROXY (IV yok): RV terim yapısı — 10-bar vs 100-bar realized vol:
//!   <0.6 — vol sıkışması (genişleme riski); >1.6 — gerileme beklentisi
//!   arada nötr
//! NOT: HL'de opsiyon yok; çıktı pozisyon değil — risk katmanı +
//! komşu stratejiler için vol-rejim bilgisi.
//! GEREKLİ VERİ: (IV mod) >=60 mum; (proxy) >=120 mum.
use crate::{closes, MarketInput, Signal, Strategy};
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
}
