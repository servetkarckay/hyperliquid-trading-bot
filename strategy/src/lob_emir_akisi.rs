//! LOB EMİR AKIŞI — defter dengesi + taker akışı ÇİFT teyidi.
//!
//! AMAÇ: emir defterinin kısa vadeli yön öngörüsü.
//! HESAPLAMA:
//! 1. Defter dengesi: üst N seviye hacim imbalance (bids-asks)/(bids+asks)
//! 2. Taker oranı: agresif alış hacmi payı (0..1)
//! 3. Momentum onayı: son 12 bar net değişim (çelişki varsa nötr)
//! 4. Karar: imbalance>+0.15 VE taker>%55 VE momentum -%1 üstü -> Long;
//!    tersler Short. Tek başına yeterli değil — çift onay kuralı.
//! GEREKLİ VERİ: defter dengesi + taker akışı + >=30 mum.
use crate::{closes, MarketInput, Signal, Strategy};

pub struct LobEmirAkisi;

impl Strategy for LobEmirAkisi {
    fn name(&self) -> &'static str {
        "LOB Emir Akışı"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let (Some(imb), Some(taker)) = (input.book_imbalance, input.taker_ratio) else {
            return Signal::Skip { reason: "defter dengesi veya taker akışı yok".into() };
        };
        let c = closes(&input.candles);
        if c.len() < 30 {
            return Signal::Skip { reason: ">=30 mum gerekli".into() };
        }
        // momentum onayı: son 12 barın net değişimi
        let chg = (c[c.len() - 1] / c[c.len() - 13] - 1.0).clamp(-0.2, 0.2);

        let strength = (imb.abs() * 0.5 + (taker - 0.5).abs() * 1.0).min(0.8);
        if imb > 0.15 && taker > 0.55 && chg > -0.01 {
            return Signal::Long {
                strength,
                note: format!("defter dengesi {imb:+.2}, taker %{:.0} alıcı — alış baskısı", taker * 100.0),
            };
        }
        if imb < -0.15 && taker < 0.45 && chg < 0.01 {
            return Signal::Short {
                strength,
                note: format!("defter dengesi {imb:+.2}, taker %{:.0} satıcı — satış baskısı", taker * 100.0),
            };
        }
        Signal::Hold { note: format!("nötr: imb={imb:+.2} taker={taker:.2}") }
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

    fn candles(n: usize) -> Vec<data::Candle> {
        H::flat(n, 100.0)
    }

    /// A — imb>+0.15 VE taker>%55 VE momentum −%1 üstü → Long.
    #[test]
    fn lob_cift_onay_long_uretir() {
        let input = MarketInput {
            coin: "T".into(),
            candles: candles(30),
            book_imbalance: Some(0.4),
            taker_ratio: Some(0.7),
            ..Default::default()
        };
        match LobEmirAkisi.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!(strength > 0.0 && strength <= 0.8);
                assert!(note.contains("alış baskısı"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

    /// C — Short simetrisi: imb<−0.15 VE taker<%45.
    #[test]
    fn lob_cift_onay_short_uretir() {
        let input = MarketInput {
            coin: "T".into(),
            candles: candles(30),
            book_imbalance: Some(-0.4),
            taker_ratio: Some(0.3),
            ..Default::default()
        };
        assert!(matches!(LobEmirAkisi.evaluate(&input), Signal::Short { strength, .. } if strength > 0.0));
    }

    /// C — ASİMETRİ ONAY YASAĞI: tek onay yeterli değil (imb pozitif ama
    /// taker nötr → Long ÜRETİLMEZ; tersi de).
    #[test]
    fn lob_tek_onay_yeterli_degil() {
        let imb_only = MarketInput { coin: "T".into(), candles: candles(30), book_imbalance: Some(0.5), taker_ratio: Some(0.5), ..Default::default() };
        assert!(matches!(LobEmirAkisi.evaluate(&imb_only), Signal::Hold { .. }), "imb tek başına Long ÜRETMEZ");
        let taker_only = MarketInput { coin: "T".into(), candles: candles(30), book_imbalance: Some(0.0), taker_ratio: Some(0.9), ..Default::default() };
        assert!(matches!(LobEmirAkisi.evaluate(&taker_only), Signal::Hold { .. }), "taker tek başına Long ÜRETMEZ");
    }

    /// D — defter/taker yoksa Skip.
    #[test]
    fn lob_veri_yoksa_skip() {
        let i1 = MarketInput { coin: "T".into(), candles: candles(30), book_imbalance: None, taker_ratio: Some(0.7), ..Default::default() };
        assert!(matches!(LobEmirAkisi.evaluate(&i1), Signal::Skip { .. }));
        let i2 = MarketInput { coin: "T".into(), candles: candles(30), book_imbalance: Some(0.4), taker_ratio: None, ..Default::default() };
        assert!(matches!(LobEmirAkisi.evaluate(&i2), Signal::Skip { .. }));
    }
}
