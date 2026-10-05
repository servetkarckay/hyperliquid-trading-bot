//! MARKET MAKING — Avellaneda-Stoikov rezervasyon fiyatı (AUXILIARY).
//!
//! Görev: quote planı — rezervasyon fiyatı `r = mid − q·γ·σ²·T`,
//! yarım spread + envanter skew'i (bps). Çıktı **Hold** — yönlü giriş
//! sinyali üretmez; gerçek parayla quote KONULMAZ (MM emir yolu motor
//! IOC'yle çelişir — bilinen sınır). σ≤0 / defter yok → Skip.
use crate::{closes, MarketInput, Signal, StratStatus, Strategy};
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

    /// Kote/quote planlayıcı — yönlü giriş sinyali üretmez → `AUXILIARY`.
    fn status(&self, _input: &MarketInput) -> StratStatus {
        StratStatus::Auxiliary
    }
}

fn realized_sigma(returns: &[f64], n: usize) -> f64 {
    let w = &returns[returns.len() - n..];
    let m = w.iter().sum::<f64>() / n as f64;
    (w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Signal;

    fn mum(t: i64, c: f64) -> data::Candle {
        data::Candle { t, o: c, h: c * 1.01, l: c * 0.99, c, v: 1.0, n: 1, closed: true }
    }

    fn input(best_bid: Option<f64>, best_ask: Option<f64>, n: usize) -> MarketInput {
        let mut i = MarketInput { best_bid, best_ask, ..Default::default() };
        i.candles = (0..n as i64).map(|k| mum(k, 100.0 * (1.0 + 0.01 * (k % 7) as f64))).collect();
        i
    }

    /// MM'nin ALO (maker) olma gereksinimi ile executor'ın global IOC kararı
    /// arasında bugün **çelişki yok** — çünkü MM hiçbir zaman yönlü sinyal
    /// üretmiyor. MM bir gün Long/Short dönmeye başlarsa bu test KIRMIZI olur
    /// ve o gün `prefer_alo` taşıyıcısı gerekir.
    #[test]
    fn hicbir_kosulda_yonlu_sinyal_uretmez() {
        let cases = [
            input(None, None, 100),      // defter yok
            input(Some(99.0), Some(101.0), 10),  // yetersiz mum
            input(Some(99.0), Some(101.0), 100), // normal: ALO quote notu
        ];
        for (idx, i) in cases.iter().enumerate() {
            let s = MarketMaking.evaluate(i);
            assert!(
                matches!(s, Signal::Hold { .. } | Signal::Skip { .. }),
                "vaka {idx}: MM yönlü sinyal üretti ({s:?}) — executor IOC kuralıyla çelişir"
            );
        }
    }

    /// Avellaneda-Stoikov: `r = mid − q·γ·σ²·T`.
    /// **Uzun envanter → r aşağı** (satışa teşvik: eldeki uzun pozisyonu
    /// bitirmek istersin) — sezgisel olarak ters görünür ama doğrudur.
    ///
    /// NOT: `q = envanter/10k USD`, bu yüzden tipik bir $500 envanterin
    /// skew'i bps'in 3. ondalığına düşer ve logda `-0.0` görünür. Test
    /// ölçülebilir olsun diye büyük envanter kullanılır.
    #[test]
    fn envanter_skew_isareti_dogru() {
        let big = 20_000_000.0; // q = 2000 → skew görünür bps
        let mut long = input(Some(99.0), Some(101.0), 100);
        long.inventory_usd = big;
        let mut short = input(Some(99.0), Some(101.0), 100);
        short.inventory_usd = -big;
        let skew = |i: &MarketInput| match MarketMaking.evaluate(i) {
            Signal::Hold { note } => note
                .split("skor ")
                .nth(1)
                .and_then(|s| s.split("bps)").next())
                .and_then(|s| s.parse::<f64>().ok())
                .expect("skor bps parse"),
            other => panic!("Hold bekleniyordu: {other:?}"),
        };
        let (l, s) = (skew(&long), skew(&short));
        assert!(l < 0.0, "uzun envanterde rezervasyon mid'in ALTINDA olmalı, {l}");
        assert!(s > 0.0, "kısa envanterde rezervasyon mid'in ÜSTÜNDE olmalı, {s}");
        assert_eq!(l, -s, "skew envanterde doğrusal ve simetrik olmalı");
    }
}
