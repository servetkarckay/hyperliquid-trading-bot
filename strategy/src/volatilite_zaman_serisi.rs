//! VOLATİLİTE ZAMAN SERİSİ — GARCH(1,1) koşullu volatilite.
//!
//! AMAÇ: volatilite kümelenmesini modelleyip yarınki volu tahmin etmek.
//! HESAPLAMA:
//! 1. Son 300 bar log getirilerine deterministik GARCH fit
//!    (varyans hedefleme ω = var·(1-α-β), koordinat inişi)
//! 2. Koşullu varyans serisi -> σ_next (yarın beklenen vol)
//! 3. Uzun dönem vol: sqrt(ω/(1-α-β))
//! 4. Karar: sıkışma oranı σ_next/σ_longrun < 0.85 -> genişleme
//!    beklentisi; yön son 24 bar momentumundan, güç sıkışma
//!    derinliğine bağlı [0-0.7]. Normal bölge Hold.
//! GEREKLİ VERİ: >=200 mum (fit güvenilirliği).
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct VolatiliteZamanSerisi;

impl Strategy for VolatiliteZamanSerisi {
    fn name(&self) -> &'static str {
        "Volatilite Zaman Serisi"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 200 {
            return Signal::Skip { reason: ">=200 mum gerekli (GARCH tahmini)".into() };
        }
        let r = indicators::log_returns(&c);

        // GARCH(1,1) fit (varyans hedeflemeli, deterministik)
        let from = r.len().saturating_sub(300);
        let (omega, alpha, beta, _ll) = indicators::garch11_fit(&r[from..], 20);
        let vars = indicators::garch11_variance(&r, omega, alpha, beta);
        let sigma_next = vars.last().unwrap().sqrt(); // yarın beklenen vol
        let sigma_longrun = (omega / (1.0 - alpha - beta).max(1e-9)).sqrt();

        // sıkışma oranı: <1 -> sıkışmış (genişleme beklentisi)
        let ratio = sigma_next / sigma_longrun;

        // yön: kısa momentum (son 24 bar)
        let mom = c[c.len() - 1] / c[c.len() - 25] - 1.0;

        if ratio < 0.85 {
            let dir = if mom > 0.0 { ("Long", mom) } else { ("Short", -mom) };
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
