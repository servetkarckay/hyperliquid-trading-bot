//! CARRY — funding taşıma + basis yardımcıları.
//!
//! 1. HL mekanizması (DOĞRULANMIŞ): API `funding` alanı **SAATLİK**
//! (fundingHistory 1h aralıklı; BTC 1.25e-5 = faiz+premium imzası);
//! funding HER SAAT ödenir; UI'daki "8s" gösterimi = hourly×8.
//! `FUNDING_ANNUALIZATION = 24×365 = 8760`.
//! 2. Yön: pozitif funding → LONG öder → SHORT carry; negatif → LONG.
//! Eşik |yıllık| > %10; altı Hold (sıfır funding Hold).
//! 3. ctx yok → Skip. Yardımcılar: `basis_pct`, `find_spot_mid`.
use crate::{MarketInput, Signal, Strategy};
use data::market_ctx::CtxState;

pub struct CarryFundingBasis;

/// funding yıllıklaştırma: API alanı **saatlik** → yearly = hourly × 24 × 365.
///
/// ESKİ (YANLIŞ) DEĞER 3.0 × 365.0 = 1095 idi: alan 8 saatlik SANILMIŞTI.
/// Empirik kanıt yukarıda; yanlış ölçek yıllık carry'yi 8× küçültüyor ve
/// eşik |%10| altındaki tüm sinyalleri SUSTURUYORDU (ölçek hatası = sahte
/// Hold üretmek).
pub const FUNDING_ANNUALIZATION: f64 = 24.0 * 365.0; // = 8760

impl Strategy for CarryFundingBasis {
    fn name(&self) -> &'static str {
        "Carry"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let Some(ctx) = &input.ctx else {
            return Signal::Skip { reason: "ctx (funding) yok".into() };
        };
        let CtxState { funding, mark_px, .. } = ctx;
        let funding_ann = funding * FUNDING_ANNUALIZATION; // yıllık % carry

 // threshold: |yıllık funding| > %10 anlamlı
 // HYPERLIQUID MEKANİZMASI: funding > 0 → long öder → SHORT receiver
        if funding_ann > 0.10 {
            Signal::Short {
                strength: (funding_ann / 0.5).min(0.8),
                note: format!("funding %{:.1} (yıllık) pozitif → LONG öder, SHORT carry toplar (mark={mark_px})", funding_ann * 100.0),
            }
        } else if funding_ann < -0.10 {
            Signal::Long {
                strength: ((-funding_ann) / 0.5).min(0.8),
                note: format!("funding %{:.1} (yıllık) negatif → SHORT öder, LONG carry toplar", funding_ann * 100.0),
            }
        } else {
            Signal::Hold { note: format!("funding %{:.2} yıllık — taşıma cazip değil", funding_ann * 100.0) }
        }
    }
}

/// Basis yardımcı (strateji 08 spot-perp): ayrı erişim için public
pub fn basis_pct(mark_px: f64, spot_mid: f64) -> Option<f64> {
    if spot_mid <= 0.0 { None } else { Some((mark_px - spot_mid) / spot_mid) }
}

/// spot mids'ten eşleşen spot çiftini arar (örn. UBTC/USDC @index)
pub fn find_spot_mid(mids: &std::collections::HashMap<String, f64>, min_value: f64) -> Option<(String, f64)> {
    mids.iter()
        .filter(|(k, v)| k.starts_with('@') && **v > min_value)
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(k, v)| (k.clone(), *v))
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;
    use data::market_ctx::CtxState;

    fn ctx(funding: f64) -> CtxState {
        CtxState { coin: "T".into(), funding, open_interest: 1.0, mark_px: 100.0, oracle_px: 100.0, premium: 0.0, day_ntl_vlm: 1_000_000.0, prev_day_px: 100.0, ts: 0 }
    }

 /// E — POZİTİF funding → LONG öder → SHORT receiver (yön doğrulaması).
    #[test]
    fn carry_funding_sign_long_short() {
 // pozitif: SHORT carry toplar
        let input = MarketInput { coin: "T".into(), ctx: Some(ctx(0.0001)), ..Default::default() };
        match CarryFundingBasis.evaluate(&input) {
            Signal::Short { strength, note } => {
                assert!(strength > 0.0 && strength <= 0.8);
                assert!(note.contains("SHORT carry toplar"), "{note}");
            }
            other => panic!("pozitif funding → Short beklenirdi: {other:?}"),
        }
 // negatif: LONG carry toplar
        let input = MarketInput { coin: "T".into(), ctx: Some(ctx(-0.0001)), ..Default::default() };
        assert!(matches!(CarryFundingBasis.evaluate(&input), Signal::Long { .. }));
 // ters yön SAHTE: pozitif funding asla Long üretmez
        let input = MarketInput { coin: "T".into(), ctx: Some(ctx(0.001)), ..Default::default() };
        assert!(!matches!(CarryFundingBasis.evaluate(&input), Signal::Long { .. }));
    }

 /// E — ÖLÇEK: API alanı SAATLİK → yıllık = ×24×365 (=8760).
 /// Eski 3×365=1095 alanı 8 saatlik SANIYORDU → carry'yi 8× küçültüp
 /// sinyalleri susturuyordu (empirik: fundingHistory 1h aralıklı).
    #[test]
    fn carry_funding_annualization_correct() {
        assert!((FUNDING_ANNUALIZATION - 8760.0).abs() < 1e-9, "saatlik → ×24×365");
 // BTC gerçek değeri 1.25e-5/h → yıllık %10.95 → eşik (|%10|) üstünde
        let ann = 1.25e-5 * FUNDING_ANNUALIZATION;
        assert!((ann - 0.1095).abs() < 1e-3, "BTC yıllık carry {ann:.4} — %10 eşiğini GEÇMELİ");
 // eşik davranışı: %11 yıllık (funding = 0.11/8760) → Short
        let f = 0.11 / FUNDING_ANNUALIZATION;
        let input = MarketInput { coin: "T".into(), ctx: Some(ctx(f)), ..Default::default() };
        assert!(matches!(CarryFundingBasis.evaluate(&input), Signal::Short { .. }), "yıllık %11 → Short");
 // %9 → Hold (eşik altı)
        let f = 0.09 / FUNDING_ANNUALIZATION;
        let input = MarketInput { coin: "T".into(), ctx: Some(ctx(f)), ..Default::default() };
        assert!(matches!(CarryFundingBasis.evaluate(&input), Signal::Hold { .. }));
    }

 /// E — funding = 0 → Hold (yön üretmez).
    #[test]
    fn carry_zero_funding_is_hold() {
        let input = MarketInput { coin: "T".into(), ctx: Some(ctx(0.0)), ..Default::default() };
        match CarryFundingBasis.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("cazip değil"), "{note}"),
            other => panic!("Hold beklenirdi: {other:?}"),
        }
    }

 /// D — ctx yok → Skip.
    #[test]
    fn carry_ctx_yoksa_skip() {
        let input = MarketInput::default();
        assert!(matches!(CarryFundingBasis.evaluate(&input), Signal::Skip { .. }));
    }
}
