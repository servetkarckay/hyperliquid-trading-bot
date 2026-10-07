//! ORDER FLOW IMBALANCE (OFI) — defter dengesi + taker akışı çift teyidi.
//!
//! KANONİK KAYNAK: Cont, Kukanov & Stoikov (2014) "The Price Impact of
//! Order Book Events" — Order Flow Imbalance (OFI) kısa vadeli fiyat
//! değişimini doğrusal açıklar (R²≈%65; Decay hızlı — mikroyapı
//! sinyali). Çok-seviye entegrasyon: Xu et al. (2022) — üst seviyelerin
//! birleşimi best-level OFI'dan daha açıklayıcı.
//!
//! BU MODÜLÜN ÖLÇÜMÜ: book_imbalance = 5-seviye toplam
//! (bid−ask)/(bid+ask) ∈ [−1,1] — çok-seviye entegre OFI'nin derinlik
//! ağırlıklı yaklaşımı (Xu et al. ruhu; tick-bazlı kanonik e_n formülü
//! DEĞİL — L2 snapshot kontratının sınırları).
//!
//! UFUK NOTU (bilinçli): OFI mikroyapı sinyalidir (saniye-dakika;
//! hızlı söner). Motor 300-sn döngü kontratında churn'ü önlemek için
//! 12-bar momentum KAPLAMASI eklenmiştir (teori değil, motor-ufuk
//! uyumu) + taker_ratio (CVD-spirit) çift onayı.
//!
//! EŞİKLER (kalibrasyon): imb ±0.15, taker %55/%45, momentum −%1.
//! Veri yoksa → Skip (sahte sinyal yok); tek onay → Hold.
use crate::{closes, MarketInput, Signal, Strategy};

pub struct OrderFlowImbalance;

impl Strategy for OrderFlowImbalance {
    fn name(&self) -> &'static str {
        "Order Flow Imbalance"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let (Some(imb), Some(taker)) = (input.book_imbalance, input.taker_ratio) else {
            return Signal::Skip {
                reason: "defter dengesi veya taker akışı yok".into(),
            };
        };
        let c = closes(&input.candles);
        if c.len() < 30 {
            return Signal::Skip {
                reason: ">=30 mum gerekli".into(),
            };
        }
        // momentum onayı: son 12 barın net değişimi
        let chg = (c[c.len() - 1] / c[c.len() - 13] - 1.0).clamp(-0.2, 0.2);

        let strength = (imb.abs() * 0.5 + (taker - 0.5).abs() * 1.0).min(0.8);
        if imb > 0.15 && taker > 0.55 && chg > -0.01 {
            return Signal::Long {
                strength,
                note: format!(
                    "defter dengesi {imb:+.2}, taker %{:.0} alıcı — alış baskısı",
                    taker * 100.0
                ),
            };
        }
        if imb < -0.15 && taker < 0.45 && chg < 0.01 {
            return Signal::Short {
                strength,
                note: format!(
                    "defter dengesi {imb:+.2}, taker %{:.0} satıcı — satış baskısı",
                    taker * 100.0
                ),
            };
        }
        Signal::Hold {
            note: format!("nötr: imb={imb:+.2} taker={taker:.2}"),
        }
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
        match OrderFlowImbalance.evaluate(&input) {
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
        assert!(
            matches!(OrderFlowImbalance.evaluate(&input), Signal::Short { strength, .. } if strength > 0.0)
        );
    }

    /// C — ASİMETRİ ONAY YASAĞI: tek onay yeterli değil (imb pozitif ama
    /// taker nötr → Long ÜRETİLMEZ; tersi de).
    #[test]
    fn lob_tek_onay_yeterli_degil() {
        let imb_only = MarketInput {
            coin: "T".into(),
            candles: candles(30),
            book_imbalance: Some(0.5),
            taker_ratio: Some(0.5),
            ..Default::default()
        };
        assert!(
            matches!(OrderFlowImbalance.evaluate(&imb_only), Signal::Hold { .. }),
            "imb tek başına Long ÜRETMEZ"
        );
        let taker_only = MarketInput {
            coin: "T".into(),
            candles: candles(30),
            book_imbalance: Some(0.0),
            taker_ratio: Some(0.9),
            ..Default::default()
        };
        assert!(
            matches!(
                OrderFlowImbalance.evaluate(&taker_only),
                Signal::Hold { .. }
            ),
            "taker tek başına Long ÜRETMEZ"
        );
    }

    /// D — defter/taker yoksa Skip.
    #[test]
    fn lob_veri_yoksa_skip() {
        let i1 = MarketInput {
            coin: "T".into(),
            candles: candles(30),
            book_imbalance: None,
            taker_ratio: Some(0.7),
            ..Default::default()
        };
        assert!(matches!(
            OrderFlowImbalance.evaluate(&i1),
            Signal::Skip { .. }
        ));
        let i2 = MarketInput {
            coin: "T".into(),
            candles: candles(30),
            book_imbalance: Some(0.4),
            taker_ratio: None,
            ..Default::default()
        };
        assert!(matches!(
            OrderFlowImbalance.evaluate(&i2),
            Signal::Skip { .. }
        ));
    }
}
