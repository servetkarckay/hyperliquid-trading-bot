//! BREAKOUT — sıkışma ön koşullu kırılım OLAY dedektörü.
//!
//! 1. Sıkışma: BB(20,2) genişliğinın kırılımdan **5 bar önceki** değeri,
//! ÖNCESİNİN 100 ölçümü içinde ilk %35'lik dilimde düzeltmesi:
//! eski kod kırılım barında ölçüyordu → yüzdelik daima %100 → ölü strateji).
//! 2. Tetik: fiyat son 30 barın zirve/dibini geçer (kırılım OLAY — yön
//! tahmini değil, iki yan simetrik).
//! 3. Onay: taker akışı (yok → Skip) + OI teyidi (yok → kırılım sinyal
//! ÜRETMEZ — Hold "OI teyidi YOK").
//! 4. ≥125 mum / BB serisi ≥105; aksi Skip.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct BreakoutKirilim;

impl Strategy for BreakoutKirilim {
    fn name(&self) -> &'static str {
        "Breakout"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 120 {
            return Signal::Skip { reason: ">=120 mum gerekli".into() };
        }
        let price = c[c.len() - 1];

 // BB genişliği serisi ve yüzdelik
        let bb = indicators::bollinger(&c, 20, 2.0);
        let width: Vec<f64> = bb
            .iter()
            .enumerate()
            .filter_map(|(i, (m, up, lo))| match (m, up, lo) {
                (Some(m), Some(u), Some(l)) if *m > 0.0 => Some((u - l) / m),
                _ => {
                    let _ = i;
                    None
                }
            })
            .collect();
        if width.len() < 105 {
            return Signal::Skip { reason: "BB genişlik serisi kısa (<105)".into() };
        }
 // DOKÜMAN-BİREBİR DÜZELTME doküman "sıkışma ön koşulu: son
 // genişlik ilk %20'lik dilimdeydi (son 5 bar öncesi)" der; kod ise
 // KIRILIM BARININ genişliğini ölçüyordu — kırılım barı BB'yi patlattığı
 // için yüzde daima ~%100 çıkıyor, sıkışma koşulu HİÇ sağlanmıyordu
 // (ölü strateji). Kural: kırılımdan 5 bar ÖNCEKİ genişlik, ÖNCESİNİN
 // 100 ölçümü içinde ilk %35'lik dilimde mi.
        if width.len() < 105 {
            return Signal::Skip { reason: format!("BB genişlik serisi kısa ({})", width.len()) };
        }
        let pre_width = width[width.len() - 6];
        let hist = &width[width.len() - 105..width.len() - 5];
        let pct = hist.iter().filter(|w| **w <= pre_width).count() as f64 / hist.len() as f64;

 // sıkışma ön koşulu: kırılımdan 5 bar önce genişlik ilk %35'lik dilimde
        let was_squeezed = pct < 0.35;

 // kırılım tetiği: fiyat son 30 barın zirvesini/dibini geçti mi
        let hh30 = c[c.len() - 31..c.len() - 1].iter().cloned().fold(f64::MIN, f64::max);
        let ll30 = c[c.len() - 31..c.len() - 1].iter().cloned().fold(f64::MAX, f64::min);
        let breakout_up = price > hh30;
        let breakout_dn = price < ll30;

 // MISSING-DATA SAFETY: onay verisi YOKSA sinyal ÜRETİLMEZ.
 // Eski fallback'ler: `taker.unwrap_or(0.5)` (nötr→her iki onay false →
 // baskılayıcı ama sessiz) ve `oi_delta.unwrap_or(0.0)` (OI'sız
 // kırılıma ONAYSIZ izin). Şimdi: taker yoksa → Skip (veri yok
 // kapsamında tek doğru cevap), OI yoksa → kırılım teyit edilemez →
 // Hold (veri gelene kadar; OI onayı notta açıkça işaretlenir).
        let Some(taker) = input.taker_ratio else {
            return Signal::Skip { reason: "taker akışı yok — onay verilemez".into() };
        };
        let taker_buy = taker > 0.55;
        let taker_sell = taker < 0.45;
        let oi = input.oi_delta;


        if was_squeezed && breakout_up && taker_buy {
            return match oi {
                Some(d) if d > 0.0 => Signal::Long {
                    strength: 0.75,
                    note: format!("sıkışma (%{pct:.0} genişlik) sonrası yukarı kırılım + taker alıcı + OI↑"),
                },
                _ => Signal::Hold {
                    note: format!("sıkışma+yukarı kırılım+taker onayı VAR ama OI teyidi YOK — sinyal onaysız"),
                },
            };
        }
        if was_squeezed && breakout_dn && taker_sell {
            return match oi {
                Some(d) if d < 0.0 => Signal::Short {
                    strength: 0.75,
                    note: "sıkışma sonrası aşağı kırılım + taker satıcı + OI↓".into(),
                },
                _ => Signal::Hold {
                    note: "sıkışma+aşağı kırılım+taker onayı VAR ama OI teyidi YOK — sinyal onaysız".into(),
                },
            };
        }
        if was_squeezed {
            Signal::Hold { note: format!("sıkışma var (%{pct:.0}), kırılım bekleniyor (HH30={hh30:.0} LL30={ll30:.0})") }
        } else {
            Signal::Hold { note: format!("sıkışma yok (genişlik %{:.0} yüzdelik)", pct * 100.0) }
        }
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

 /// 120 mum; genişlik AZALAN (sıkışma): close = 100 ± a(i), a(i) = 1 − 0.005·i.
 /// Kırılım barı (119): 112 (yukarı) veya 88 (aşağı).
    fn squeeze_series(breakout_close: f64) -> Vec<data::Candle> {
        let mut c: Vec<data::Candle> = (0..125)
            .map(|i| {
                let a = 1.0 - 0.005 * i as f64;
                let px = 100.0 + a * (if i % 2 == 0 { 1.0 } else { -1.0 });
                H::candle(i as i64 * 3_600_000, px, px + 0.1, px - 0.1, px, 100.0)
            })
            .collect();
        H::set_last(&mut c, breakout_close, breakout_close + 0.1, breakout_close - 0.1, 200.0);
        c
    }

 /// A — sıkışma + yukarı kırılım + taker alıcı + OI↑ → Long.
    #[test]
    fn breakout_sikisma_yukari_oi_onayli_long() {
        let c = squeeze_series(112.0);
        let input = MarketInput {
            coin: "T".into(),
            candles: c,
            taker_ratio: Some(0.6),
            oi_delta: Some(1.5),
            ..Default::default()
        };
        match BreakoutKirilim.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!((strength - 0.75).abs() < 1e-9);
                assert!(note.contains("OI↑"), "OI onayı notta: {note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

 /// C — Short simetrisi: aşağı kırılım + taker satıcı + OI↓.
    #[test]
    fn breakout_sikisma_asagi_oi_onayli_short() {
        let c = squeeze_series(88.0);
        let input = MarketInput {
            coin: "T".into(),
            candles: c,
            taker_ratio: Some(0.4),
            oi_delta: Some(-1.5),
            ..Default::default()
        };
        match BreakoutKirilim.evaluate(&input) {
            Signal::Short { strength, note } => {
                assert!((strength - 0.75).abs() < 1e-9);
                assert!(note.contains("OI↓"), "{note}");
            }
            other => panic!("Short beklenirdi: {other:?}"),
        }
    }

 /// D — taker verisi YOKSA Skip; OI verisi YOKSA kırılım ONAYSIZ →
 /// sinyal ÜRETİLMEZ (eski fallback'ler bunu sessizce geçiriyordu).
    #[test]
    fn breakout_veri_yoksa_sahte_sinyal_uretmez() {
        let c = squeeze_series(112.0);
        let yok_taker = MarketInput { coin: "T".into(), candles: c.clone(), taker_ratio: None, oi_delta: Some(1.5), ..Default::default() };
        assert!(matches!(BreakoutKirilim.evaluate(&yok_taker), Signal::Skip { .. }), "taker yok → Skip");
        let yok_oi = MarketInput { coin: "T".into(), candles: c.clone(), taker_ratio: Some(0.6), oi_delta: None, ..Default::default() };
        match BreakoutKirilim.evaluate(&yok_oi) {
            Signal::Hold { note } => assert!(note.contains("OI teyidi YOK"), "{note}"),
            other => panic!("OI'sız kırılım sinyal ÜRETMEZ (Hold beklenirdi): {other:?}"),
        }
    }

 /// B — FUTURE LEAKAGE: kırılım barı görülmeden Long ÜRETİLMEZ
 /// (sıkışma koşulu bile kırılımdan 5 bar ÖNCEKİ genişlikle ölçülür);
 /// gelecek 50 bar prefix kararını değiştirmez.
    #[test]
    fn breakout_future_leak_prefix_karar_degismez() {
        let mut full = squeeze_series(112.0);
        full.extend(H::flat(50, 112.0)); // gelecek 50 bar
 // A = kırılım ÖNCESİ (bar 0..=118)
        let a = full[..124].to_vec();
        let ia = MarketInput { coin: "T".into(), candles: a.clone(), taker_ratio: Some(0.6), oi_delta: Some(1.5), ..Default::default() };
        let sa = BreakoutKirilim.evaluate(&ia);
        assert!(!matches!(sa, Signal::Long { .. }), "kırılım barı görülmeden Long üretilmez: {sa:?}");
 // t anı (kırılım barı 119) → Long
        let it = MarketInput { coin: "T".into(), candles: full[..125].to_vec(), taker_ratio: Some(0.6), oi_delta: Some(1.5), ..Default::default() };
        assert!(matches!(BreakoutKirilim.evaluate(&it), Signal::Long { .. }), "kırılım barında Long");
 // prefix karar sabit (saf fonksiyon)
        assert_eq!(sa, BreakoutKirilim.evaluate(&ia));
    }
}
