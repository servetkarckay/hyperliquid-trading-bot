//! OPTİMAL YÜRÜTME — parent emir parçalama planı.
//!
//! AMAÇ: piyasa etkisi ile zamanlama arasındaki denge.
//! KURALLAR:
//!   - "vol yüksekse hızlı yürüt, emir büyükse yavaş yürüt"
//!   - parça boyutu defter ilk seviye derinliğine bağlı
//!   - U-şekilli trajectory: başta/sonda hızlı, ortada yavaş
//! HESAPLAMA: parça sayısı = sqrt(parent/derinlik), kısıt [3,20];
//! aciliyet = bar volatilitesi (Ioc/Alo seçimi ~2.5 eşiği); parça
//! ağırlıkları U-trajektörüyle orantılı. Sinyal: Hold + plan notta
//! (parça sayısı, USD/parça, önerilen TIF, ilk 5 parça).
//! GEREKLİ VERİ: order_size_usd + >=50 mum.
use crate::{closes, MarketInput, Signal, Strategy};
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
}

fn realized_sigma(returns: &[f64], n: usize) -> f64 {
    let w = &returns[returns.len() - n..];
    let m = w.iter().sum::<f64>() / n as f64;
    (w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
}
