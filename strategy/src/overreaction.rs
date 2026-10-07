//! OVERREACTION — aşırı tepki tersine-dönüşü (davranışsal finans).
//!
//! KANONİK KAYNAK: De Bondt & Thaler (1985) "Does the Stock Market
//! Overreact?" — Journal of Finance. İki hipotez (makale birebir):
//! 1. **Aşırı fiyat hareketleri, ters yönde hareketler takip eder.**
//! 2. **Başlangıç hareketi ne kadar aşırıysa, düzeltme o kadar büyük
//!    olur.** → güç ölçeklemesi |σ| ile doğru orantılıdır (kod: |σ|/4,
//!    cap 0.8 — kalibrasyon).
//!
//! UFUK NOTU (bilinçli): kanonik orijinal 36 AYLIK oluşum dönemidir
//! (loser/winner portföyleri; dönüş 5 yıla dek). Bu modül kısa ufuk
//! uygulamasıdır — Lehmann (1990) haftalık tersine-dönüş ruhu: 6-bar
//! ekstrem getiri + hacim teyidi. Ufuk kısa; hipotezler aynı.
//!
//! TETİK (kalibrasyon): 6-bar getiri, 100-bar log-return RMS ile
//! normalize (z-benzeri; gerçek std hesabı değil) — |σ| > 2 VE hacim
//! 2× spike (panik/euphoria hacmi teyit katmanı). Tetik yok → Hold.
//!
//! YÖN: panik satışı → kontra Long; euphoria → kontra Short
//! (hipotez 1'in simetrik uygulaması).
//!
//! <100 mum → Skip; bölenler 1e-9 korumalı; deterministik; panik yok.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct Overreaction;

impl Strategy for Overreaction {
    fn name(&self) -> &'static str {
        "Overreaction"
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

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

 /// A — panik satışı: 6 barda <-%8 (2σ+) + hacim 2x → kontra Long.
    #[test]
    fn davranissal_panic_kontra_long() {
        let mut c = H::flat(114, 100.0);
 // 6 barda sert düşüş + hacim patlaması
        for i in 0..6 {
            let px = 100.0 - (i + 1) as f64 * 2.0;
            H::set(&mut c, 108 + i, px, px + 0.2, px - 0.5, 400.0);
        }
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Overreaction.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!(strength > 0.0 && strength <= 0.8);
                assert!(note.contains("aşırı tepki"), "{note}");
            }
            other => panic!("kontra Long beklenirdi: {other:?}"),
        }
    }

 /// C — Short simetrisi: euphoria (6 barda > 2σ + hacim) → kontra Short.
    #[test]
    fn davranissal_euphoria_kontra_short() {
        let mut c = H::flat(114, 100.0);
        for i in 0..6 {
            let px = 100.0 + (i + 1) as f64 * 2.0;
            H::set(&mut c, 108 + i, px, px + 0.5, px - 0.2, 400.0);
        }
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Overreaction.evaluate(&input) {
            Signal::Short { strength, note } => {
                assert!(strength > 0.0 && strength <= 0.8);
                assert!(note.contains("euphoria"), "{note}");
            }
            other => panic!("kontra Short beklenirdi: {other:?}"),
        }
    }

 /// D — anormal hareket yok → Hold (tetik yoksa sinyal yok).
    #[test]
    fn davranissal_normal_dongu_hold() {
        let candles = H::flat(120, 100.0);
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        match Overreaction.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("anormal hareket yok"), "{note}"),
            other => panic!("Hold beklenirdi: {other:?}"),
        }
    }
}
