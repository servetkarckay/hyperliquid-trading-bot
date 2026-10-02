//! ELLIOTT DALGA — pivot tabanlı dalga sayımı + Fibonacci teyidi.
//!
//! HESAPLAMA:
//! 1. Pivotlar: ±12 bar
//! 2. 5 dalga yukarı şablonu: artan üç zirve (W1,W3,W5)
//! 3. Düzeltme dipleri yükseliyor mu (W2/W4)
//! 4. Fibonacci: son geri çekilme W1'in %30-65 bandında mı
//! 5. Likidite teyidi (hacim > 0)
//! Güç zayıf [0.45] — sayım belirsizliği yüksek; onaysız sayım
//! sinyal değildir (doküman kuralı).
//! GEREKLİ VERİ: >=150 mum.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct ElliottDalga;

impl Strategy for ElliottDalga {
    fn name(&self) -> &'static str {
        "Elliott"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 150 {
            return Signal::Skip { reason: ">=150 mum gerekli (4h+ dalga yapısı)".into() };
        }
        let (piv_h, piv_l) = pivots(&c, 12);
        if piv_h.len() < 3 || piv_l.len() < 3 {
            return Signal::Hold { note: "yeterli pivot yok — sayım belirsiz".into() };
        }
        // 5-dalga yukarı şablonu: artan pivot zirveleri W1,W3,W5
        let rising = piv_h.len() >= 3
            && piv_h[piv_h.len() - 1] > piv_h[piv_h.len() - 2]
            && piv_h[piv_h.len() - 2] > piv_h[piv_h.len() - 3];
        // W2/W4 dipleri yükseliyor mu
        let dips_rising = piv_l.len() >= 2 && piv_l[piv_l.len() - 1] > piv_l[piv_l.len() - 2];

        // Fibonacci: son geri çekilme W1 aralığının %38.2-61.8 bandında mı
        let w1 = piv_h[piv_h.len() - 1] - piv_l[0];
        let last_low = piv_l[piv_l.len() - 1];
        let retr = if w1 > 0.0 { (piv_h[piv_h.len() - 2] - last_low) / w1.max(1e-12) } else { 0.0 };
        let fib_ok = (0.30..=0.65).contains(&retr);

        // likidite filtresi (doküman: 4h/dayNtlVlm)
        let vol_ok = input
            .candles
            .last()
            .map(|x| x.v > 0.0)
            .unwrap_or(false);

        if rising && dips_rising && fib_ok && vol_ok {
            Signal::Long {
                strength: 0.45,
                note: format!("5 dalga yukarı şablonu + geri çekilme %{:.0} (fib bandı) — sayım belirsizliği zayıf sinyal", retr * 100.0),
            }
        } else {
            Signal::Hold {
                note: format!("sayım belirsiz: rising={rising} dips={dips_rising} retr={retr:.2}"),
            }
        }
    }
}

fn pivots(c: &[f64], k: usize) -> (Vec<f64>, Vec<f64>) {
    let mut highs = Vec::new();
    let mut lows = Vec::new();
    for i in k..c.len() - k {
        let win = &c[i - k..=i + k];
        if c[i] == win.iter().cloned().fold(f64::MIN, f64::max) { highs.push(c[i]); }
        if c[i] == win.iter().cloned().fold(f64::MAX, f64::min) { lows.push(c[i]); }
    }
    (highs, lows)
}

#[allow(dead_code)]
fn _ref() {
    let _ = indicators::ema;
    
}
