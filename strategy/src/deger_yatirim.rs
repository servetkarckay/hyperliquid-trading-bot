//! DEĞER YATIRIM — temel analiz proxy modu.
//!
//! Gerçek temel skor (MC/TVL, TVL trend, net real yield) HARİCİDİR —
//! HL API'de yok. `evaluate_with_fundamentals()` skoru hazır:
//! 0.30·MC/TVL + 0.25·trend + 0.25·yield — harici feed gelince aktif.
//! HL'den hesaplanan proxy: dayNtlVlm/OI oranı = likidite kalitesi.
//! Sinyal: Hold + proxy raporu notta.
//! GEREKLİ VERİ: canlı ctx.
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
