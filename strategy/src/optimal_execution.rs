//! OPTIMAL EXECUTION — parent emir parçalama planı (AUXILIARY).
//!
//! Görev: U-şekilli trajectory — parça sayısı = sqrt(parent/derinlik),
//! kısıt [3,20]; aciliyet (bar volu) → Ioc/Alo önerisi; parça ağırlıkları
//! notta. `order_size_usd ≤ 0` → Skip (engine tahsis sonrası planladığı
//! için evaluation'a beslenemez — strateji AUXILIARY işaretli, sessiz
//! ölü değil).
//!
//! **BİLİNCİ Lİ SINIR (ölü entegrasyon yolu)**: etkin yol (a) parent-order
//! planner'ının motor sonrası bağlanması veya (b) modülün araştırma
//! ayrımına taşınmasıdır; bugün registry'de AUXILIARY + raporda öyle
//! görünür — sahte alpha kanıtı ÜRETMEZ.
use crate::{closes, MarketInput, Signal, StratStatus, Strategy};
use math::indicators;

pub struct OptimalExecution;

impl Strategy for OptimalExecution {
    fn name(&self) -> &'static str {
        "Optimal Execution"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        if input.order_size_usd <= 0.0 {
            return Signal::Skip { reason: "parent emir yok".into() };
        }
        let c = closes(&input.candles);
        if c.len() < 50 {
            return Signal::Skip { reason: ">=50 mum gerekli".into() };
        }
        let r = indicators::log_returns(&c);
        let sigma = realized_sigma(&r, 50); // bar volatilitesi
        let depth_usd: f64 = input
            .best_bid
            .map(|_| 50_000.0) // tahmini ilk seviye derinliği; canlıda l2Book'tan
            .unwrap_or(50_000.0);

 // parça sayısı: büyük emir + yüksek vol -> daha çok parça
        let urgency = (sigma * 100.0).clamp(0.5, 5.0); // vol ne kadar yüksek o kadar hızlı
        let slices = ((input.order_size_usd / depth_usd.max(1.0f64)).sqrt().ceil() as usize)
            .clamp(3, 20);
        let slice_usd = input.order_size_usd / slices as f64;
        let tif = if urgency > 2.5 { "Ioc" } else { "Alo (post-only)" };

 // U-şekilli: başta hızlı, ortada yavaş, sonda hızlı (Almgren-Chriss rehberi)
        let weights: Vec<f64> = (0..slices)
            .map(|i| {
                let x = i as f64 / (slices - 1).max(1) as f64;
                0.5 + (2.0 * x - 1.0).powi(2)
            })
            .collect();
        let total: f64 = weights.iter().sum();
        let plan: Vec<String> = weights
            .iter()
            .enumerate()
            .take(5)
            .map(|(i, w)| format!("#{}: {:.0}$", i + 1, w / total * input.order_size_usd))
            .collect();

        Signal::Hold {
            note: format!(
                "plan: {} parça × ~{:.0}$, tif={tif}, aciliyet={urgency:.1} | ilk parçalar [{}]",
                slices, slice_usd, plan.join(", ")
            ),
        }
    }

 /// Parent-emir planlayıcı — yönlü giriş sinyali üretmez → `AUXILIARY`.
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
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

 /// A — parent emir planı: plan notu üretilir (parça sayısı/USD).
    #[test]
    fn optexec_plan_uretir() {
        let input = MarketInput { coin: "T".into(), candles: H::flat(60, 100.0), order_size_usd: 50_000.0, ..Default::default() };
        match OptimalExecution.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("parça") || note.contains('#'), "{note}"),
            other => panic!("plan Hold beklenirdi: {other:?}"),
        }
    }

 /// D — order_size_usd yoksa Skip (ölü strateji yasağı: engine bu alanı
 /// besleyemez → strateji AUXILIARY işaretli, sessiz ölü değil).
    #[test]
    fn optexec_parent_yoksa_skip() {
        let input = MarketInput { coin: "T".into(), candles: H::flat(60, 100.0), order_size_usd: 0.0, ..Default::default() };
        match OptimalExecution.evaluate(&input) {
            Signal::Skip { reason } => assert!(reason.contains("order_size") || reason.contains("emir"), "{reason}"),
            other => panic!("Skip beklenirdi: {other:?}"),
        }
        assert_eq!(OptimalExecution.status(&input), StratStatus::Auxiliary, "auxiliary işaretli");
    }
}
