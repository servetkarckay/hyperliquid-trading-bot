//! DEĞER YATIRIM — temel analiz PROXY modu.
//!
//! 1. **SINIR (dürüst)**: gerçek temel skorlar (MC/TVL, TVL trend, net
//!    real yield) HL API'de YOK — harici feed bekliyor.
//! 2. `evaluate_with_fundamentals`: feed geldiğinde doküman skoru
//!    (0.30 MC/TVL + 0.25 trend + 0.25 yield); >0.6 Long, <0.3 Skip
//!    (value trap), arası Hold.
//! 3. `evaluate` (şu anki canlı yol): HL proxy — günlük hacim/OI oranı
//!    raporu; Hold — YÖN ÜRETMEZ (SAHTE temel skor UYDURULMAZ);
//!    ctx yok → Skip.
use crate::{MarketInput, Signal, Strategy};
use data::market_ctx::CtxState;

pub struct DegerYatirim;

/// Harici temel skorlar (indexleyiciden beslenir)
#[derive(Debug, Clone, Copy)]
pub struct FundamentalScores {
    /// MC/TVL (düşük = ucuz), TVL trendi (+/-), net real yield (oran)
    pub mc_tvl: f64,
    pub tvl_trend: f64,
    pub real_yield: f64,
}

impl DegerYatirim {
    pub fn evaluate_with_fundamentals(&self, s: FundamentalScores) -> Signal {
        // doküman skoru: 0.30 MC/TVL + TVL trend + real yield
        let score = 0.30 * (5.0 - s.mc_tvl).clamp(0.0, 5.0) / 5.0
            + 0.25 * (s.tvl_trend + 0.2).clamp(0.0, 0.4) / 0.4
            + 0.25 * s.real_yield.clamp(0.0, 0.2) / 0.2;
        if score > 0.6 {
            Signal::Long { strength: score.min(0.9), note: format!("temel skor {score:.2}: MC/TVL={:.2} trend={:+.0}% yield={:.1}%", s.mc_tvl, s.tvl_trend * 100.0, s.real_yield * 100.0) }
        } else if score < 0.3 {
            Signal::Skip { reason: format!("temel skor düşük {score:.2} — value trap riski") }
        } else {
            Signal::Hold { note: format!("temel skor orta: {score:.2}") }
        }
    }
}

impl Strategy for DegerYatirim {
    fn name(&self) -> &'static str {
        "Değer Yatırım"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        // HL'den hesaplanabilen tek proxy: likidite kalitesi (dayNtlVlm, OI)
        match &input.ctx {
            Some(CtxState { day_ntl_vlm, open_interest, mark_px, .. }) => {
                let oi_usd = open_interest * mark_px;
                let liq_ratio = day_ntl_vlm.max(1.0) / oi_usd.max(1.0);
                Signal::Hold {
                    note: format!(
                        "temel veri harici (TVL/staking yok); HL proxy: günlük hacim/OI={liq_ratio:.2} — temel skor için feed bağlanın"
                    ),
                }
            }
            None => Signal::Skip { reason: "ctx yok".into() },
        }
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;
    use data::market_ctx::CtxState;

    /// A — temel skor yolu: yüksek skor → Long; düşük → Skip.
    #[test]
    fn deger_temel_skor_yolu() {
        let iyi = DegerYatirim.evaluate_with_fundamentals(FundamentalScores { mc_tvl: 0.5, tvl_trend: 0.2, real_yield: 0.2 });
        match iyi {
            Signal::Long { strength, .. } => assert!(strength > 0.6 && strength <= 0.9),
            other => panic!("iyi temeller → Long: {other:?}"),
        }
        let kotu = DegerYatirim.evaluate_with_fundamentals(FundamentalScores { mc_tvl: 5.0, tvl_trend: -0.5, real_yield: 0.0 });
        assert!(matches!(kotu, Signal::Skip { .. }), "value trap → Skip: {kotu:?}");
        let orta = DegerYatirim.evaluate_with_fundamentals(FundamentalScores { mc_tvl: 2.5, tvl_trend: 0.0, real_yield: 0.1 });
        assert!(matches!(orta, Signal::Hold { .. }));
    }

    /// D — HL proxy modu: temel veri YOK → Hold (yön üretmez, SAHTE
    /// temel skor UYDURULMAZ) + ctx yoksa Skip.
    #[test]
    fn deger_proxy_yon_uretmez() {
        let ctx = CtxState { coin: "T".into(), funding: 0.0, open_interest: 100.0, mark_px: 2.0, oracle_px: 2.0, premium: 0.0, day_ntl_vlm: 500.0, prev_day_px: 2.0, ts: 0 };
        let input = MarketInput { coin: "T".into(), ctx: Some(ctx), ..Default::default() };
        match DegerYatirim.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("harici"), "{note}"),
            other => panic!("proxy Hold beklenirdi: {other:?}"),
        }
        assert!(matches!(DegerYatirim.evaluate(&MarketInput::default()), Signal::Skip { .. }));
    }
}
