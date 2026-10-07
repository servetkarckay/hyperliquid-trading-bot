//! TURTLE SYSTEM — System 1 (kanonik: Original Turtle Rules, Curtis Faith).
//!
//! Kanonik System 1 kuralları (kaynak: "The Original Turtle Rules" PDF):
//! 1. **GİRİŞ**: fiyat, önceki 20 barın (kanonik: 20 gün; burada bar
//!    ünitesi) en üstünü aşarsa 1 Unit LONG, en dibi aşarsa 1 Unit
//!    SHORT. Kanal ÖNCEKİ barların kanalıdır (breakout konvansiyonu).
//! 2. **S1 FİLTRESİ — kayıp-kırılım kuralı**: son kırılım KAZANAN
//!    sayılırsa yeni 20-bar giriş önemsizdir (atlanır). Kırılımın
//!    yönü IRRELEVANT (kazanma/kaybetme yönünüz bağımsız sayılır).
//!    Kırılım KAYIP sayılır: kırılımdan itibaren fiyat, 2N KARŞI
//!    yöne sarkarsa (N = 20-bar ATR) — karlı bir 10-bar çıkışı
//!    gerçekleşmeden önce. Son kırılım çözülmemiş (-trade'te)
//!    ise kazanmış sayılır → yeni giriş atlanır (üst üste
//!    pozisyonlamayı da önler).
//! 3. **FAILSAFE 55-BAR KIRILIMI**: filter yüzünden atlanan 20-bar
//
//!    (büyük hareketleri kaçırmamak için; "Failsafe Breakout").
//! 4. **ÇIKIŞ (S1)**: 10-bar adverse breakout — LONG pozisyon, fiyat
//!    önceki 10 barın diplerin altına inerse çıkış; SHORT pozisyon,
//!    fiyat önceki 10 barın zirvelerinin üstüne çıkarsa çıkış.
//!    Kural `exit_intent` TRAIT API'siyle stratejide yaşar (yön
//!    farkındalığı motor state'ine bağlıdır); motor yalnız
//!    yönlendirir. **İki taraf simetrik** — önceki long-only
//!    sınırlama kaldırıldı.
//! 5. **BOYUT/STOP (motorum konatı)**: Unit = %1 equity / N; stop
//!    gap = 2N; her +0.5N karda ½N-added piramit (max 4 unit) —
//!    bunlar motorun sizing katmanına aittir; bu strateji YALNIZ
//!    karar (yön + filtre + çıkış) üretir, boyut üretmez.
//!
//! S2 (55-bar kanonik bağımsız sistem) uygulanmaz: 55-bar kırılımı
//! yalnız FAILSAFE giriş olarak kullanılır (bilinçli sınır — bkz.
//! docs "Dürüst sınırlar").
//!
//! Deterministik, replay-idempotent, panik yok; yetersiz veri → Skip.
use crate::{closes, highs, lows, MarketInput, Signal, Strategy, ExitCtx, ExitIntent, ExitPositionInfo};
use math::indicators;

pub struct TurtleSystem;

const MIN_BARS: usize = 85; // 20 kanal + 60 tarama + 10 çıkış penceresi
const ENTRY_CHANNEL: usize = 20; // System 1 kanonik: 20-day
const EXIT_CHANNEL: usize = 10; // System 1 kanonik: 10-day adverse
const LOSE_MULT: f64 = 2.0; // kayıp kırılım: 2N karşı hareket
const FILTER_LOOKBACK: usize = 60; // son kırılım tarama aralığı
const FAILSAFE_CHANNEL: usize = 55; // failsafe kırılım
const STRENGTH: f64 = 0.7; // motor sizing katmanına karar gücü konvansiyonu

enum Outcome {
    Winner,
    Loser,
    Unresolved,
}

 /// Son KIRILIMIN sonucu (kayıp-kırılım filtresi) — stateless tarama:
 /// most-recent descubrimiento barını bulur, kapatılana kadar o
 /// kırılımdan beri 2N-against ya da 10-bar-exit var mı bakar.
fn last_breakout_outcome(
    h: &[f64],
    l: &[f64],
    c: &[f64],
    atr: &[Option<f64>],
    n_len: usize,
) -> (Option<usize>, Outcome) {
    // most-recent önce (n-2'den geriye): son bar o AN'de yeni kırılım
    let mut found: Option<(usize, bool)> = None;
    for k in (EXIT_CHANNEL + 1..n_len - 1).rev() {
        if k < ENTRY_CHANNEL + 1 {
            break;
        }
        let hh = h[k - ENTRY_CHANNEL..k].iter().cloned().fold(f64::MIN, f64::max);
        let ll = l[k - ENTRY_CHANNEL..k].iter().cloned().fold(f64::MAX, f64::min);
        if c[k] > hh {
            found = Some((k, true));
            break;
        }
        if c[k] < ll {
            found = Some((k, false));
            break;
        }
    }
    let Some((t0, is_long_entry)) = found else {
        return (None, Outcome::Unresolved);
    };
    let Some(n0) = atr.get(t0).and_then(|x| *x) else {
        return (Some(t0), Outcome::Unresolved);
    };
    if !(n0.is_finite() && n0 > 0.0) {
        return (Some(t0), Outcome::Unresolved);
    }
    let entry0 = c[t0];
    for m in (t0 + 1)..n_len {
 // karlı 10-bar çıkışı S1 kuralı: fiyat önceki 10 barın ucunu KAPANIŞLA ihlal et.
        if m >= EXIT_CHANNEL {
            let window = &l[m - EXIT_CHANNEL..m];
            if is_long_entry {
                let exit_level = window.iter().cloned().fold(f64::MAX, f64::min);
                if c[m] < exit_level {
                    return (Some(t0), if c[m] > entry0 { Outcome::Winner } else { Outcome::Loser });
                }
            } else {
                let window_h: &[f64] = &h[m - EXIT_CHANNEL..m];
                let exit_level = window_h.iter().cloned().fold(f64::MIN, f64::max);
                if c[m] > exit_level {
                    return (Some(t0), if entry0 > c[m] { Outcome::Winner } else { Outcome::Loser });
                }
            }
        }
 // 2N-against: kayıp kırılım kanunu
        if is_long_entry {
            if l[m] < entry0 - LOSE_MULT * n0 {
                return (Some(t0), Outcome::Loser);
            }
        } else if h[m] > entry0 + LOSE_MULT * n0 {
            return (Some(t0), Outcome::Loser);
        }
    }
    (Some(t0), Outcome::Unresolved)
}

impl Strategy for TurtleSystem {
    fn name(&self) -> &'static str {
        "Turtle"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let (h, l, c) = (highs(&input.candles), lows(&input.candles), closes(&input.candles));
        let n = c.len();
        if n < MIN_BARS {
            return Signal::Skip { reason: format!("System 1 taraması için >={MIN_BARS} mum gerekli") };
        }
        let atr_series = indicators::atr(&h, &l, &c, ENTRY_CHANNEL);
        let Some(ncur) = atr_series.last().and_then(|x| *x) else {
            return Signal::Skip { reason: "ATR hesaplanamadı".into() };
        };
        if !(ncur.is_finite() && ncur > 0.0) {
            return Signal::Skip { reason: "ATR geçersiz".into() };
        }
        let price = c[n - 1];

 // KANAL kanonik: önceki 20 barın zirve/dibi (son bar hariç)
        let hh20 = h[n - 1 - ENTRY_CHANNEL..n - 1].iter().cloned().fold(f64::MIN, f64::max);
        let ll20 = l[n - 1 - ENTRY_CHANNEL..n - 1].iter().cloned().fold(f64::MAX, f64::min);
        let break_up = price > hh20;
        let break_down = price < ll20;
        if !break_up && !break_down {
            return Signal::Hold {
                note: format!(
                    "kanal içi ({price:.4}) — 20-bar kanal {hh20:.4}/{ll20:.4}; N(ATR20)={ncur:.4}"
                ),
            };
        }

 // S1 FİLTRESİ: son kırılımın sonucu (kazanan girişler eleme)
        let (_, outcome) = last_breakout_outcome(&h, &l, &c, &atr_series, n);
        let last_winner = matches!(outcome, Outcome::Winner | Outcome::Unresolved);

        if last_winner {
 // FAILSAFE: atlanan 20-bar giriş yerine 55-bar kanal kırılırmsa girilir
            let hh55 = h[n - 1 - FAILSAFE_CHANNEL..n - 1]
                .iter()
                .cloned()
                .fold(f64::MIN, f64::max);
            let ll55 = l[n - 1 - FAILSAFE_CHANNEL..n - 1]
                .iter()
                .cloned()
                .fold(f64::MAX, f64::min);
            if break_up && price > hh55 {
                return Signal::Long {
                    strength: STRENGTH,
                    note: format!(
                        "failsafe 55-bar kırılımı: {price:.4} > {hh55:.4} (S1 filtresi: son 20-bar kırılım kazandı)"
                    ),
                };
            }
            if break_down && price < ll55 {
                return Signal::Short {
                    strength: STRENGTH,
                    note: format!(
                        "failsafe 55-bar kırılımı: {price:.4} < {ll55:.4} (S1 filtresi: son 20-bar kırılım kazandı)"
                    ),
                };
            }
            return Signal::Hold {
                note: format!(
                    "S1 filtresi: son kırılım kazandı — 20-bar giriş atlanır; failsafe 55-bar: {hh55:.4}/{ll55:.4}"
                ),
            };
        }

 // kayıp/ilk kırılım → kanonik System 1 girilş validated
        if break_up {
            return Signal::Long {
                strength: STRENGTH,
                note: format!(
                    "20-bar kırılım: {price:.4} > {hh20:.4}; S1 filtre: son kırılım kaybetti/ilk; N(ATR20)={ncur:.4}"
                ),
            };
        }
        Signal::Short {
            strength: STRENGTH,
            note: format!(
                "20-bar alt kırılım: {price:.4} < {ll20:.4}; S1 filtre: son kırılım kaybetti/ilk; N(ATR20)={ncur:.4}"
            ),
        }
    }

 /// S1 ÇIKIŞ (kanonik): 10-bar adverse breakout — LONG: önceki 10 barın
 /// dip ortalamasinin değil, DIPLERİN altına KAPANIŞ; SHORT: zirve üze.
 /// İki taraf simetrik; Flat pozisyon → niyet üretilmez.
    fn exit_intent(&self, ctx: &ExitCtx) -> Option<ExitIntent> {
        let pos = ctx.position.as_ref()?;
        let l: Vec<f64> = ctx.candles.iter().map(|x| x.l).collect();
        let h: Vec<f64> = ctx.candles.iter().map(|x| x.h).collect();
        let c: Vec<f64> = ctx.candles.iter().map(|x| x.c).collect();
        if c.len() < EXIT_CHANNEL + 1 {
            return None;
        }
        let price = c[c.len() - 1];
        if pos.is_long {
            let exit_level = l[l.len() - 1 - EXIT_CHANNEL..l.len() - 1]
                .iter()
                .cloned()
                .fold(f64::MAX, f64::min);
            if price < exit_level {
                return Some(ExitIntent {
                    note: format!(
                        "S1 çıkış: 10-bar kanal altı ({price:.4} < {exit_level:.4}) — adverse breakout"
                    ),
                });
            }
        } else {
            let exit_level = h[h.len() - 1 - EXIT_CHANNEL..h.len() - 1]
                .iter()
                .cloned()
                .fold(f64::MIN, f64::max);
            if price > exit_level {
                return Some(ExitIntent {
                    note: format!(
                        "S1 çıkış: 10-bar kanal üstü ({price:.4} > {exit_level:.4}) — adverse breakout"
                    ),
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod turtle_system_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

    /// Geçmiş breakout YOK (flat rozet) + son bar 20-bar kırılımı: filtre
    /// geçer (son kırılım yok → sahipsiz kabul), Long.
    #[test]
    fn s1_20bar_kirilim_long() {
        let mut c = H::flat(85, 100.0);
        H::set_last(&mut c, 112.0, 112.5, 111.5, 100.0);
        let input = MarketInput { coin: "T".into(), candles: c, equity: 10_000.0, ..Default::default() };
        match TurtleSystem.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert_eq!(strength, STRENGTH);
                assert!(note.contains("20-bar kırılım"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }



    /// S1 ÇIKIŞ KANONİK: LONG + önceki 10 barlai diptin ALTINA kapanış →
    /// niyet VAR (55-bar değil — 10-bar adverse breakout).
    #[test]
    fn exit_intent_10bar_alti_long() {
        let mut candles = H::flat(85, 100.0);
        H::set_last(&mut candles, 80.0, 80.5, 79.5, 100.0);
        let long = ExitCtx {
            coin: "ADA".into(),
            candles,
            universe_returns: vec![],
            position: Some(ExitPositionInfo { is_long: true, size: 1.0, entry_px: 100.0 }),
        };
        let intent = TurtleSystem.exit_intent(&long).expect("10-bar altı LONG → niyet");
        assert!(intent.note.contains("10-bar"), "{intent:?}");

 // SHORT pozisyon + simetrik kuralı TETİKLEYEN mumlar (zirve ÜSTÜ) → niyet GELMELi
        let mut spike = H::flat(85, 100.0);
        H::set_last(&mut spike, 120.0, 120.5, 119.5, 100.0);
        let short = ExitCtx {
            coin: "ADA".into(),
            candles: spike,
            universe_returns: vec![],
            position: Some(ExitPositionInfo { is_long: false, size: 1.0, entry_px: 100.0 }),
        };
        assert!(
            TurtleSystem.exit_intent(&short).is_some(),
            "SHORT pozisyonda 10-bar üst kırılım → niyet GERMELİDİR (simetrik S1)"
        );
        // kanal içi → YOK
        let flat = ExitCtx {
            coin: "ADA".into(),
            candles: H::flat(85, 100.0),
            universe_returns: vec![],
            position: Some(ExitPositionInfo { is_long: true, size: 1.0, entry_px: 100.0 }),
        };
        assert!(TurtleSystem.exit_intent(&flat).is_none(), "kanal içi → niyet yok");
        // FLAT → YOK
        let flat_none = ExitCtx {
            coin: "ADA".into(),
            candles: H::flat(85, 100.0),
            universe_returns: vec![],
            position: None,
        };
        assert!(TurtleSystem.exit_intent(&flat_none).is_none());
    }

    /// prior kırılım ÇÖZÜLMEMİŞ (üst üste rampa; 10-bar kapanış eksiliş
    /// yok, 2N-against yok) → kazan部 sayılır; yeni 20 bar kırılımı hem
    /// 55-bar kanalı da kırar → FAILSAFE girişi üretilir (kazanç
    /// kaçırma koruması — kanonik).
    #[test]
    fn failsafe_ramp_uzeri_55bar_kirilimi() {
        let mut c = H::flat(51, 100.0);
        for i in 51..85 {
            let px = 100.8 + 0.3 * (i - 51) as f64;
            let prev_close = 100.8 + 0.3 * (i - 52) as f64;
            // rampa: low HEPI 100.3 üstünde (stop altına yok), close her
            // bar önceki 20-bar zirvesini aşar (yeni kırılım her bar) →
            // outcome her daimı çözülmez (winner sayılır) → son 20-breakout
            // S1 filtresine takılır → 55-bar kanal kırılınca FAILSAFE girilir
            c.push(H::candle(i as i64 * 3_600_000, prev_close, px + 0.15, px - 0.15, px, 100.0));
        }
        // final: 111.8 > hh20(110.55) ve > hh55(110.55) → failsafe
        H::set_last(&mut c, 111.8, 112.0, 111.6, 100.0);
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match TurtleSystem.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert_eq!(strength, STRENGTH);
                assert!(note.contains("failsafe"), "{note}");
                assert!(note.contains("S1 filtresi") || note.contains("kazandı"), "{note}");
            }
            other => panic!("Long failsafe beklenirdi: {other:?}"),
        }
    }

    /// prior SHORT kırılım KAYBETTİ (rebound 2N-üstü) → yeni LONG girişi
    /// S1 filtresinden GEÇER (kırılım sonuç yönü IRRELEVANT — kanonik).
    #[test]
    fn loser_prior_kirilim_yeni_giris_gecer() {
        let mut c = H::flat(85, 100.0);
        // idx41: SHORT kırılım (c=96.5 < ll20prev=99.9);
        // TR: 3.8 → ATR(20) ≈ (19×0.2+3.8)/20 ≈ 0.37 → 2N ≈ 0.74 → stop 97.24
        c[41] = H::candle(41 * 3_600_000, 100.0, 100.0, 96.2, 96.5, 100.0);
        // idx42..83: rebound lows 99.9/yükseli highs 100.1 → h > 97.24 →
        // prior 2N-against → LOSER (yön bağımsız: long girilghıts)
        // son bar: 112.5 > hh20 (100.1) → 20-bar LONG kırılım
        H::set_last(&mut c, 112.5, 112.9, 112.1, 100.0);
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match TurtleSystem.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert_eq!(strength, STRENGTH);
                assert!(note.contains("kaybetti"), "{note}");
            }
            other => panic!("Long beklenirdi (prior loser): {other:?}"),
        }
    }

    /// yetersiz veri → Skip.
    #[test]
    fn s1_yetersiz_mum_skip() {
        let input = MarketInput { coin: "T".into(), candles: H::flat(60, 100.0), ..Default::default() };
        assert!(matches!(TurtleSystem.evaluate(&input), Signal::Skip { .. }));
    }

    /// IDEMPOTENT: aynı seri → aynı sinyal.
    #[test]
    fn ayn_seri_ayni_sinyal() {
        let c = H::flat(85, 100.0);
        let i1 = MarketInput { coin: "T".into(), candles: c.clone(), ..Default::default() };
        let i2 = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        assert_eq!(TurtleSystem.evaluate(&i1), TurtleSystem.evaluate(&i2));
    }
}
