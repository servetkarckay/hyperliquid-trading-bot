//! VOLATİLİTE ZAMAN SERİSİ — GARCH(1,1) koşullu vol sıkışması.
//!
//! 1. GARCH(1,1) deterministik fit → σ_next / σ_long-run oranı.
//! 2. **METODOLOJİ NOTU**: bu saf GARCH stratejisi DEĞİL — vol sıkışması
//!    + 24-bar momentum KOMPOZİTİDİR; tracker'da ayrı alpha kümesi.
//! 3. Oran <0.85 (sıkışma) + kısa momentum işareti → Long/Short
//!
//! [(0.85−ratio)×2, ≤0.7] — genişleme + yön.
//! 3. <200 mum veya varyans bozuk → Skip (SAHTE tahmin yok).
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct GarchRegime;

impl Strategy for GarchRegime {
    fn name(&self) -> &'static str {
        "GARCH"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 200 {
            return Signal::Skip {
                reason: ">=200 mum gerekli (GARCH tahmini)".into(),
            };
        }
        let r = indicators::log_returns(&c);

        // GARCH(1,1) fit (varyans hedeflemeli, deterministik)
        let from = r.len().saturating_sub(300);
        let (omega, alpha, beta, _ll) = indicators::garch11_fit(&r[from..], 20);
        let vars = indicators::garch11_variance(&r, omega, alpha, beta);
        // D: varyans serisi boş/NaN → Skip (unwrap panik yasağı)
        let Some(v) = vars.last().copied().filter(|x| x.is_finite() && *x > 0.0) else {
            return Signal::Skip {
                reason: "GARCH varyansı hesaplanamadı".into(),
            };
        };
        let sigma_next = v.sqrt(); // yarın beklenen vol
        let sigma_longrun = (omega / (1.0 - alpha - beta).max(1e-9)).sqrt();

        // sıkışma oranı: <1 -> sıkışmış (genişleme beklentisi)
        let ratio = sigma_next / sigma_longrun;

        // yön: kısa momentum (son 24 bar)
        let mom = c[c.len() - 1] / c[c.len() - 25] - 1.0;

        if ratio < 0.85 {
            let dir = if mom > 0.0 {
                ("Long", mom)
            } else {
                ("Short", -mom)
            };
            return match dir.0 {
                "Long" => Signal::Long {
                    strength: ((0.85 - ratio) * 2.0).min(0.7),
                    note: format!("vol sıkışmış (ratio={ratio:.2}), GARCH ω={omega:.2e} α={alpha:.2} β={beta:.2} — genişleme + momentum yukarı"),
                },
                _ => Signal::Short {
                    strength: ((0.85 - ratio) * 2.0).min(0.7),
                    note: format!("vol sıkışmış (ratio={ratio:.2}) — genişleme + momentum aşağı"),
                },
            };
        }
        Signal::Hold {
            note: format!("vol normal bölgede (ratio={ratio:.2}), σ_next={sigma_next:.5}"),
        }
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

    /// A — sıkışma + pozitif momentum → Long; negatif → Short (simetri).
    #[test]
    fn volzaman_sikisma_momentum_yonu() {
        // geniş→dar geçiş: ilk 100 bar geniş, son 100 bar dar
        let mut c = H::oscillator(220, |i| if i < 110 { 3.0 } else { 0.15 }, 100.0, 0.05);
        // son bar momentum yukarı
        H::set_last(&mut c, 101.0, 101.2, 100.8, 100.0);
        let input = MarketInput {
            coin: "T".into(),
            candles: c.clone(),
            ..Default::default()
        };
        match GarchRegime.evaluate(&input) {
            Signal::Long { strength, .. } => assert!(strength > 0.0 && strength <= 0.7),
            other => panic!("sıkışma+momentum↑ → Long: {other:?}"),
        }
        // momentum aşağı → Short
        let mut c2 = c;
        H::set_last(&mut c2, 99.0, 99.2, 98.8, 100.0);
        let input2 = MarketInput {
            coin: "T".into(),
            candles: c2,
            ..Default::default()
        };
        match GarchRegime.evaluate(&input2) {
            Signal::Short { strength, .. } => assert!(strength > 0.0 && strength <= 0.7),
            other => panic!("sıkışma+momentum↓ → Short: {other:?}"),
        }
    }

    /// D — <200 mum → Skip; GARCH varyans bozuksa Skip (unwrap panik yok).
    #[test]
    fn volzaman_veri_yoksa_skip() {
        let input = MarketInput {
            coin: "T".into(),
            candles: H::flat(100, 100.0),
            ..Default::default()
        };
        assert!(matches!(GarchRegime.evaluate(&input), Signal::Skip { .. }));
    }
}
