//! DAVRANIŞSAL FİNANS — aşırı tepki (overreaction) geri alımı.
//!
//! AMAÇ: kayıp tetiklenen panik satışları (loss aversion) kısa vadede
//! geri döner; euphoria alımları soğur.
//! HESAPLAMA:
//!   6-bar getiri, 100-bar σ ile normalize edilir (<-2σ / >+2σ)
//!   + hacim 20-bar ortalamanın 2x üstü = tetik
//!   panik -> kontra Long [0-0.8]; euphoria -> kontra Short
//!   tetiklenmediyse Hold.
//! GEREKLİ VERİ: >=100 mum.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct DavranissalFinans;

impl Strategy for DavranissalFinans {
    fn name(&self) -> &'static str {
        "Davranışsal"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 100 {
            return Signal::Skip { reason: ">=100 mum gerekli".into() };
        }
        let vols: Vec<f64> = input.candles.iter().map(|x| x.v).collect();
        let v_avg: f64 = vols[vols.len() - 20..].iter().sum::<f64>() / 20.0;
        let v_last = vols[vols.len() - 1];
        let vol_spike = v_last / v_avg.max(1e-12);

        // 6 barlık aşırı hareket (z-score benzeri normalize)
        let move6 = c[c.len() - 1] / c[c.len() - 7] - 1.0;
        let r = indicators::log_returns(&c);
        let sd = {
            let w = &r[r.len().saturating_sub(100)..];
            (w.iter().map(|x| x * x).sum::<f64>() / w.len() as f64).sqrt()
        };
        let normalized = move6 / (sd * 6.0f64.sqrt()).max(1e-9);

        // panik: 6 barda <-%8 (2σ+ üstü) + hacim 2x -> kontra LONG
        if normalized < -2.0 && vol_spike > 2.0 {
            Signal::Long {
                strength: ((-normalized) / 4.0).min(0.8),
                note: format!("aşırı tepki satışı: {move6:+.1}% ({normalized:.1}σ), hacim {vol_spike:.1}x — panik geri alınır"),
            }
        } else if normalized > 2.0 && vol_spike > 2.0 {
            Signal::Short {
                strength: (normalized / 4.0).min(0.8),
                note: format!("euphoria alımı: {move6:+.1}% ({normalized:.1}σ), hacim {vol_spike:.1}x — soğuma beklentisi"),
            }
        } else {
            Signal::Hold { note: format!("anormal hareket yok: {normalized:.1}σ, hacim {vol_spike:.1}x") }
        }
    }
}
