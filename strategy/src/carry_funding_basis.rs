//! CARRY — funding oranı taşıma + basis yardımcıları.
//!
//! AMAÇ: pozitif funding'te long perp funding KAZANIR; negatifte short
//! perp carry toplar.
//! HESAPLAMA:
//!   yıllık funding = funding × 3 × 365 (8 saatlik periyot)
//!   eşik: |yıllık| > %10 anlamlı; pozitif -> Long [0-0.8],
//!   negatif -> Short; nötr Hold
//! Yardımcılar: basis_pct(mark, spot_mid) — perp-spot sapması;
//! find_spot_mid — allMids'ten anlamlı USD spot çifti.
//! GEREKLİ VERİ: canlı ctx (funding/mark) — data::MarketCtx.
use crate::{MarketInput, Signal, Strategy};
use data::market_ctx::CtxState;

pub struct CarryFundingBasis;

/// funding yıllıklaştırma: 8 saatlik periyot × 3 × 365
const FUNDING_ANNUALIZATION: f64 = 3.0 * 365.0;

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

        // threshold: |yillik funding| > %10 anlamlı
        if funding_ann > 0.10 {
            Signal::Long {
                strength: (funding_ann / 0.5).min(0.8),
                note: format!("funding %{:.1} (yıllık) pozitif — long perp carry toplar (mark={mark_px})", funding_ann * 100.0),
            }
        } else if funding_ann < -0.10 {
            Signal::Short {
                strength: ((-funding_ann) / 0.5).min(0.8),
                note: format!("funding %{:.1} (yıllık) negatif — short perp carry toplar", funding_ann * 100.0),
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
