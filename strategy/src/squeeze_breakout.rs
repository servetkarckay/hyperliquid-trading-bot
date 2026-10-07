//! SQUEEZE BREAKOUT — Bollinger Method I (kanonik uygulama).
//!
//! Kaynak: Bollinger Bands Method I ("Squeeze Breakout") — kanonik
//! kurallar:
//! 1. BandWidth = (upperBB − lowerBB) / middleBB; BB(20, 2.0).
//! 2. **SQUEEZE**: BandWidth'in 125-periyot DÜŞÜKÜ (kanonik lookback);
//!    default %3 buffer — BandWidth ≤ 125-düşüğü × 1.03 = sıkışmada.
//!    Kırılım barı bandı genişlettiği için ölçüm kırılımdan 5 bar
//!    ÖNCEKİ genişlikle yapılır (o noktadaki 125-periyot düşüğüne göre).
//! 3. **TETİK**: %b = (close − lowerBB) / (upperBB − lowerBB);
//!    %b > 1.0 → üst bant ÜSTÜNE kapanış → Long;
//!    %b < 0.0 → alt bant ALTINA kapanış → Short.
//! 4. **VOLATİLİTE DÖNGÜSÜ** (Method I'ın dayanağı): düşük volatilite
//!    yüksek volatiliteyi doğurur — squeeze → expansion.
//!
//! VENUE GÜVENLİK KATMANI (borsa-özel; teori değil, veri dürüstlüğü):
//! - taker akışı YOKSA → Skip (onay verilemez; sahte yön yasak).
//! - taker yönlü EŞLEŞMİYORSA (yukarı kırılım ama satıcı baskın) → Hold.
//! - OI teyidi YOKSA → Hold "OI teyidi YOK" (kırılım onaysız).
//! - OI teyidi: yukarı kırılım + OI↑ / aşağı kırılım + OI↓ → kabul.
//!
//! Deterministik, replay-idempotent, panik yok; yetersiz veri → Skip.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct SqueezeBreakout;

const MIN_BARS: usize = 150; // 125-genişlik penceresi + BB(20) ısınma
const BB_N: usize = 20;
const BB_K: f64 = 2.0;
const SQUEEZE_LOOKBACK: usize = 125; // kanonik: 125-periyot düşük
const SQUEEZE_BUFFER: f64 = 1.03; // Method I default %3 buffer
const SQUEEZE_LAG: usize = 5; // sıkışma ölçümü kırılımdan kaç bar önce

impl Strategy for SqueezeBreakout {
    fn name(&self) -> &'static str {
        "Squeeze Breakout"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < MIN_BARS {
            return Signal::Skip { reason: format!("şema için >={MIN_BARS} mum gerekli") };
        }
        let bb = indicators::bollinger(&c, BB_N, BB_K);
        // BandWidth serisi: (u − l) / m
        let width: Vec<f64> = bb
            .iter()
            .filter_map(|(m, u, l)| match (m, u, l) {
                (Some(m), Some(u), Some(l)) if *m > 0.0 => Some((u - l) / m),
                _ => None,
            })
            .collect();
        if width.len() < SQUEEZE_LOOKBACK {
            return Signal::Skip {
                reason: format!("BandWidth serisi kısa (<{SQUEEZE_LOOKBACK})"),
            };
        }
        // ── SQUEEZE: kırılımdan SQUEEZE_LAG bar önceki genişlik,
        // o noktadaki 125-periyot düşüğünün %3 buffer içinde mi
        let w_idx = width.len() - 1 - SQUEEZE_LAG;
        let win_start = w_idx + 1 - SQUEEZE_LOOKBACK;
        if win_start < 0 {
            return Signal::Skip { reason: "sıkışma penceresi eksik".into() };
        }
        let win_start = win_start as usize;
        let window = &width[win_start..=w_idx];
        let min125 = window.iter().cloned().fold(f64::MAX, f64::min);
        let pre = width[w_idx];
        let squeezed = pre <= min125 * SQUEEZE_BUFFER;
        let ratio = pre / min125.max(1e-12);

        // ── TETİK: %b bant kırılımı (kırılım barı = SON bar)
        let Some(tuple) = bb.last().copied() else {
            return Signal::Skip { reason: "BB serisi boş".into() };
        };
        let (Some(m), Some(u), Some(l)) = tuple else {
            return Signal::Skip { reason: "BB hesaplanamaz".into() };
        };
        let _ = m;
        let band = u - l;
        if !(band > 0.0) || !band.is_finite() {
            return Signal::Skip { reason: "BB aralığı yok".into() };
        }
        let price = c[c.len() - 1];
        let b_pct = (price - l) / band;
        let breakout_up = b_pct > 1.0;
        let breakout_dn = b_pct < 0.0;

        // ── VENUE GÜVENLİK KATMANI
        let Some(taker) = input.taker_ratio else {
            return Signal::Skip { reason: "taker akışı yok — onay verilemez".into() };
        };
        let taker_buy = taker > 0.55;
        let taker_sell = taker < 0.45;
        let oi = input.oi_delta;

        if squeezed && breakout_up {
            if !taker_buy {
                return Signal::Hold {
                    note: format!(
                        "yukarı bant kırılımı (%b={b_pct:.2}) ama taker alıcı YOK ({taker:.2}) — onaysız"
                    ),
                };
            }
            return match oi {
                Some(d) if d > 0.0 => Signal::Long {
                    strength: 0.75,
                    note: format!(
                        "sıkışma sonrası üst bant kırılımı (%b={b_pct:.2}, 125-düşük %{:.0}) + taker alıcı + OI↑",
                        ratio * 100.0
                    ),
                },
                _ => Signal::Hold {
                    note: format!("sıkışma+yukarı kırılım+taker onayı VAR ama OI teyidi YOK — sinyal onaysız"),
                },
            };
        }
        if squeezed && breakout_dn {
            if !taker_sell {
                return Signal::Hold {
                    note: format!(
                        "aşağı bant kırılımı (%b={b_pct:.2}) ama taker satıcı YOK ({taker:.2}) — onaysız"
                    ),
                };
            }
            return match oi {
                Some(d) if d < 0.0 => Signal::Short {
                    strength: 0.75,
                    note: format!(
                        "sıkışma sonrası alt bant kırılımı (%b={b_pct:.2}, 125-düşük %{:.0}) + taker satıcı + OI↓",
                        ratio * 100.0
                    ),
                },
                _ => Signal::Hold {
                    note: "sıkışma+aşağı kırılım+taker onayı VAR ama OI teyidi YOK — sinyal onaysız".into(),
                },
            };
        }
        if squeezed {
            Signal::Hold {
                note: format!(
                    "sıkışma var (125-düşük %{:.0}), bant kırılımı bekleniyor (UBB={u:.4} LBB={l:.4})",
                    ratio * 100.0
                ),
            }
        } else {
            Signal::Hold {
                note: format!("sıkışma yok (BandWidth 125-düşüğün %{:.0}'i)", ratio * 100.0),
            }
        }
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

    /// 125 mum; genişlik AZALAN → PLATO (sıkışmada): a(i) = min(1−0.005i, 0.40).
    /// Kırılım barı (son): bant üstü/altı kapanış.
    fn squeeze_series(breakout_close: f64) -> Vec<data::Candle> {
        let mut c = H::oscillator(150, |i| (1.0 - 0.005 * i as f64).min(0.40), 100.0, 0.1);
        H::set_last(&mut c, breakout_close, breakout_close + 0.1, breakout_close - 0.1, 200.0);
        c
    }

    /// sıkışma YOK: genişlik BÜYÜYEN (min125 geçmişte çok küçük).
    fn bulge_series(breakout_close: f64) -> Vec<data::Candle> {
        let mut c: Vec<data::Candle> = (0..150)
            .map(|i| {
                let a = 2.0 + 0.01 * i as f64;
                let px = 100.0 + a * (if i % 2 == 0 { 1.0 } else { -1.0 });
                H::candle(i as i64 * 3_600_000, px, px + 0.1, px - 0.1, px, 100.0)
            })
            .collect();
        H::set_last(&mut c, breakout_close, breakout_close + 0.1, breakout_close - 0.1, 200.0);
        c
    }

    fn onayli(c: Vec<data::Candle>) -> MarketInput {
        MarketInput {
            coin: "T".into(),
            candles: c,
            taker_ratio: Some(0.6),
            oi_delta: Some(1.5),
            ..Default::default()
        }
    }

    /// A — sıkışma + üst bant üstü kapanış (%b>1) + taker alıcı + OI↑ → Long.
    #[test]
    fn squeeze_ust_bant_kirilim_long() {
        let input = onayli(squeeze_series(112.0));
        match SqueezeBreakout.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert_eq!(strength, 0.75);
                assert!(note.contains("OI↑"), "OI onayı notta: {note}");
                assert!(note.contains("%b="), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

    /// C — Short simetrisi: alt bant altı kapanış (%b<0) + taker satıcı + OI↓.
    #[test]
    fn squeeze_alt_bant_kirilim_short() {
        let input = MarketInput {
            coin: "T".into(),
            candles: squeeze_series(88.0),
            taker_ratio: Some(0.4),
            oi_delta: Some(-1.5),
            ..Default::default()
        };
        match SqueezeBreakout.evaluate(&input) {
            Signal::Short { strength, note } => {
                assert_eq!(strength, 0.75);
                assert!(note.contains("OI↓"), "{note}");
            }
            other => panic!("Short beklenirdi: {other:?}"),
        }
    }

    /// Tetik kanoniktir: 30-bar zirve kırılımı %b>1 DEĞİLSE sinyal yok —
    /// fiyat bant üstünde DEĞİLSE (kırılım barı değil) Long üretilmez.
    #[test]
    fn tetik_bb_bantidir() {
        // sıkışma serisi + bant İÇİNDE kalan close (112 değil 100.6 → %b<1)
        let input = onayli(squeeze_series(100.6));
        match SqueezeBreakout.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("bekleniyor") || note.contains("%b"), "{note}"),
            other => panic!("Hold beklenirdi: {other:?}"),
        }
    }

    /// sıkışma YOK (BandWidth bulge'ta) → bant kırılımı olsa bile sinyal YOK.
    #[test]
    fn bulge_kirilim_sinyal_uretmez() {
        let input = onayli(bulge_series(112.0));
        match SqueezeBreakout.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("sıkışma yok"), "{note}"),
            other => panic!("Hold beklenirdi: {other:?}"),
        }
    }

    /// D — taker YOKSA Skip; OI YOKSA kırılım onaysız → Hold.
    #[test]
    fn veri_yoksa_sahte_sinyal_uretmez() {
        let c = squeeze_series(112.0);
        let yok_taker = MarketInput { coin: "T".into(), candles: c.clone(), taker_ratio: None, oi_delta: Some(1.5), ..Default::default() };
        assert!(matches!(SqueezeBreakout.evaluate(&yok_taker), Signal::Skip { .. }), "taker yok → Skip");
        let yok_oi = MarketInput { coin: "T".into(), candles: c.clone(), taker_ratio: Some(0.6), oi_delta: None, ..Default::default() };
        match SqueezeBreakout.evaluate(&yok_oi) {
            Signal::Hold { note } => assert!(note.contains("OI teyidi YOK"), "{note}"),
            other => panic!("OI'sız kırılım sinyal ÜRETMEZ: {other:?}"),
        }
        // taker TERS yönde (yukarı kırılım ama satıcı): onaysız Hold
        let ters_taker = MarketInput { coin: "T".into(), candles: c, taker_ratio: Some(0.4), oi_delta: Some(1.5), ..Default::default() };
        match SqueezeBreakout.evaluate(&ters_taker) {
            Signal::Hold { note } => assert!(note.contains("taker alıcı YOK"), "{note}"),
            other => panic!("ters taker onaysız Hold: {other:?}"),
        }
    }

    /// B — FUTURE LEAKAGE: kırılım barı görülmeden Long ÜRETİLMEZ;
    /// gelecek 50 bar prefix kararını değiştirmez.
    #[test]
    fn future_leak_prefix_karar_degismez() {
        let mut full = squeeze_series(112.0);
        full.extend(H::flat(50, 112.0));
        let a = full[..149].to_vec();
        let ia = MarketInput { coin: "T".into(), candles: a.clone(), taker_ratio: Some(0.6), oi_delta: Some(1.5), ..Default::default() };
        let sa = SqueezeBreakout.evaluate(&ia);
        assert!(!matches!(sa, Signal::Long { .. }), "kırılım barı görülmeden Long üretilmez: {sa:?}");
        let it = MarketInput { coin: "T".into(), candles: full[..150].to_vec(), taker_ratio: Some(0.6), oi_delta: Some(1.5), ..Default::default() };
        assert!(matches!(SqueezeBreakout.evaluate(&it), Signal::Long { .. }), "kırılım barında Long");
        assert_eq!(sa, SqueezeBreakout.evaluate(&ia), "prefix karar sabit");
    }
}
