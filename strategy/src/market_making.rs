//! MARKET MAKING — Avellaneda-Stoikov rezervasyon fiyatı.
//!
//! AMAÇ: envantere göre quote'ları kaç birim kaydırmak gerektiği.
//! FORMÜLLER:
//!   rezervasyon r = mid - q·γ·σ²·T
//!   yarım spread = γ·σ²·T/2 + ln(1+γ/κ)/γ
//!   q = envanter/10k USD (işaretli), γ=0.1, κ=1.5, T=1 bar
//! HESAPLAMA: σ — son 50 bar log-getiri std (bar bazlı).
//! Sinyal: Hold — quote koordinatları notta (bid/ask, r, skor bps, ALO).
//! Hyperliquid'de MM yalnız ALO (post-only) quote'larla çalışır;
//! taker'a dolan her emir spread kazancını eritir (fee katmanı kuralı).
//! GEREKLİ VERİ: defter bid/ask + >=50 mum.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct MarketMaking;

impl Strategy for MarketMaking {
    fn name(&self) -> &'static str {
        "Market Making"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let (Some(bid), Some(ask)) = (input.best_bid, input.best_ask) else {
            return Signal::Skip { reason: "defter yok".into() };
        };
        let c = closes(&input.candles);
        if c.len() < 50 {
            return Signal::Skip { reason: ">=50 mum gerekli (σ tahmini)".into() };
        }
        let r = indicators::log_returns(&c);
        // 1h bar -> yıllıklaştırmasız bar-bazı σ (AS T=1 bar)
        let sigma = realized_sigma(&r, 50);
        if sigma <= 0.0 {
            return Signal::Skip { reason: "σ hesaplanamadı".into() };
        }
        let gamma = 0.1; // risk iştahı (inventory birimi başına)
        let kappa = 1.5; // dolum yoğunluğu
        let q = input.inventory_usd / 10_000.0; // envanter (10k USD = 1 birim)
        let t = 1.0;

        let mid = (bid + ask) / 2.0;
        let reservation = mid - q * gamma * sigma * sigma * t;
        let half_spread = gamma * sigma * sigma * t / 2.0
            + (1.0 + gamma / kappa).ln() / gamma;
        let skew_bps = (reservation - mid) / mid * 10_000.0;

        Signal::Hold {
            note: format!(
                "quote: bid={:.2} ask={:.2} | r={:.2} (skor {skew_bps:+.1}bps) half_spread={:.2} — ALO quote'lar r etrafında",
                bid, ask, reservation, half_spread
            ),
        }
    }
}

fn realized_sigma(returns: &[f64], n: usize) -> f64 {
    let w = &returns[returns.len() - n..];
    let m = w.iter().sum::<f64>() / n as f64;
    (w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
}
