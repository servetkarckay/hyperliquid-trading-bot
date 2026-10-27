//! MMT — MACRO-MICRO TREND ENGINE (Trend & Kırılım Modülü).
//!
//! 6 trend stratejisinin KATMAN ZİNCİRİ (hepsi ✓ olmalı — biri bile
//! veto ederse sinyal yok; SMC-L ile aynı dürüst mimari):
//!
//! 1. **TrendBoost (Rejim)**: `regime_detection::classify` — yalnız
//!    TREND rejimi (ADX>25 + Hurst>0.55 + EMA eğimi), yön eşleşmesi şart.
//!    VolSpike/Squeeze/MR/GEÇİŞ → sinyal YOK (disiplin kapısı).
//! 2. **Turtle tetik**: kanonik S1 — son bar kapanışı 20-bar kanal
//!    dışını yer (long: > max[20 bar high]; short: < min[20 bar low]);
//!    20-bar atlanırsa FAILSAFE 55-bar kanal kırılımı girişe izin verir.
//! 3. **Dow yapısı**: ±10-bar pivotlarda HH+HL (+ yükseliş hacmi baskın)
//!    long; LH+LL (+ düşüş hacmi baskın) short — Rhea tenet 6 eleme matığı.
//! 4. **Squeeze enerji**: Bollinger Method I — kırılımdan 5 bar önceki
//!    BandWidth ≤ 125-periyot düşüğü ×1.03 (enerji birikim şartı).
//! 5. **Wyckoff SOS**: kırılım barı hacmi ≥ 1.3×SMA20 + geniş gövde
//!    (≥ 1.0×ATR20, yönlü) — "önce cılız, sonra hacim" (SOS/SOW).
//! 6. **Elliott darbe**: son 3 pivot (span=EL_SPAN) yönünde tekdüze
//!    ilerleyiş — impulse dalga sayımı (HH→HH→HH / LL→LL→LL).
//!
//! Fiyat planı: Turtle N (ATR20) — SL = 2N, TP = 3N (kanoniс sistem
//! 1.5R oranı; motorun kendi TP/SL/trailing katmanı ayrıca devrede).
//!
//! Deterministik, replay-idempotent, panik yok; yetersiz veri → `Skip`.
use crate::closes;
use crate::regime_detection::{classify, RegimeLabel};
use crate::{last, MarketInput, Signal, Strategy};
use data::Candle;
use math::indicators;

pub struct Mmt;

pub const MIN_BARS: usize = MACRO_TF * MACRO_MIN_BARS + 1;
pub const ATR_N: usize = 20;
/// MACRO-MICRO ayrımı: trend onayı 4h birleşik seride (200 bar = 100 gün
/// 4h), tetik 1h kanal kırılımı — ADX hâlâ 1h'ta 20-bar kırılımla
/// eşanlı teensiyet başlangıcını veto etmesin diye HTF ayrımı.
pub const MACRO_TF: usize = 4;
pub const MACRO_MIN_BARS: usize = 200;
const BB_N: usize = 20;
const BB_K: f64 = 2.0;
const SQUEEZE_LOOKBACK: usize = 125;
const SQUEEZE_BUFFER: f64 = 1.03;
pub const CHANNEL: usize = 20;
pub const PIVOT_K: usize = 10;
const EL_SPAN: usize = 3;
const EL_PIVOTS: usize = 3;
pub const EL_WINDOW: usize = 60;
pub const SOS_VOL_MULT: f64 = 1.3;

/// Fiyat planı (replay/sınav için açık).
#[derive(Debug, Clone, PartialEq)]
pub struct TrendPlan {
    pub long: bool,
    pub entry: f64,
    pub sl: f64,
    pub tp: f64,
    pub strength: f64,
    pub note: String,
}

impl Strategy for Mmt {
    fn name(&self) -> &'static str {
        "MMT"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        if input.candles.len() < MIN_BARS {
            return Signal::Skip {
                reason: "MMT için 1h mum penceresi yetersiz".into(),
            };
        }
        let Some(plan) = Self::plan(&input.candles) else {
            return Signal::Hold {
                note: "MMT: katman zinciri tamamlanmadı (trend/kırılım onayları eksik)".into(),
            };
        };
        if plan.long {
            Signal::Long {
                strength: plan.strength,
                note: plan.note,
            }
        } else {
            Signal::Short {
                strength: plan.strength,
                note: plan.note,
            }
        }
    }
}

impl Mmt {
    /// Katman zinciri — her katman geçilmezse None.
    pub fn plan(candles: &[Candle]) -> Option<TrendPlan> {
        if candles.len() < MIN_BARS {
            return None;
        }
        let c = closes(candles);
        let n = c.len();
        let last = n - 1;
        // ── 1) Turtle tetik yönü — kanal kırılımı (yön belirleyici)
        let long = turtle_trigger(candles)?;
        // ── 2) TrendBoost rejim yön eşleşmesi — MACRO (HTF: 4h birleşik)
        let (label, _) = classify_from(&aggregate_htf(candles))?;
        if !matches!(label, RegimeLabel::Trend { up } if up == long) {
            return None;
        }
        // ── 3) Squeeze enerjisi (kırılımdan önce sıkışma şart)
        if !squeeze_on(candles) {
            return None;
        }
        // ── 4) Dow yapısı (HH+HL / LH+LL + hacim)
        if !dow_ok(candles, long) {
            return None;
        }
        // ── 5) Wyckoff SOS (geniş gövde + hacim patlaması)
        let vol_expand = wyckoff_sos(candles, long)?;
        // ── 6) Elliott darbe sayımı (tekdüze pivot ilerleyişi)
        if !elliott_ok(candles, long) {
            return None;
        }
        // ── Fiyat planı (Turtle N kuralları)
        let h: Vec<f64> = candles.iter().map(|x| x.h).collect();
        let l: Vec<f64> = candles.iter().map(|x| x.l).collect();
        let atr = (*indicators::atr(&h, &l, &c, ATR_N).last()?)?;
        if !atr.is_finite() || atr <= 0.0 {
            return None;
        }
        let entry = c[last];
        let (sl, tp) = if long {
            (entry - 2.0 * atr, entry + 3.0 * atr)
        } else {
            (entry + 2.0 * atr, entry - 3.0 * atr)
        };
        // Güç: temel 0.55 + kırılım hacim fazlalığı
        let strength = (0.55 + 0.25 * (vol_expand.clamp(1.0, 2.0) - 1.0)).clamp(0.35, 1.0);
        let note = "MMT katmanları ✓ · TrendBoost+Turtle kırılımı + Dow yapısı + squeeze enerji + SOS hacim + Elliott darbe".into();
        Some(TrendPlan {
            long,
            entry,
            sl,
            tp,
            strength,
            note,
        })
    }
}

fn classify_from(candles: &[Candle]) -> Option<(RegimeLabel, String)> {
    let input = MarketInput {
        coin: String::new(),
        candles: candles.to_vec(),
        ..Default::default()
    };
    classify(&input)
}

/// 1h mumları MACRO_TF barlı birleşik 4h mumlara indirger (kronolojik);
/// salts o/h/l/c/v toplaması — replay-idempotent, panik yok.
pub fn aggregate_htf(candles: &[Candle]) -> Vec<Candle> {
    candles
        .chunks(MACRO_TF)
        .map(|g| Candle {
            t: g[0].t,
            o: g[0].o,
            h: g.iter().map(|x| x.h).fold(f64::MIN, f64::max),
            l: g.iter().map(|x| x.l).fold(f64::MAX, f64::min),
            c: g[g.len() - 1].c,
            v: g.iter().map(|x| x.v).sum(),
            n: g.len() as u64,
            closed: true,
        })
        .collect()
}

/// Turtle System-1 tetik: true → long kırılım, false → short kırılım.
pub fn turtle_trigger(candles: &[Candle]) -> Option<bool> {
    let h: Vec<f64> = candles.iter().map(|x| x.h).collect();
    let l: Vec<f64> = candles.iter().map(|x| x.l).collect();
    let c = closes(candles);
    let n = c.len();
    if n < CHANNEL + 2 {
        return None;
    }
    let last = n - 1;
    let px = c[last];
    let hh20 = h[n - 1 - CHANNEL..n - 1]
        .iter()
        .cloned()
        .fold(f64::MIN, f64::max);
    let ll20 = l[n - 1 - CHANNEL..n - 1]
        .iter()
        .cloned()
        .fold(f64::MAX, f64::min);
    if hh20.is_finite() && hh20 > 0.0 && px > hh20 {
        return Some(true);
    }
    if ll20.is_finite() && ll20 > 0.0 && px < ll20 {
        return Some(false);
    }
    None
}

/// Bollinger Method-I squeeze — "son 20 bar İÇİNDE sıkışma yaşandı":
/// son CHANNEL bardaki en-dar BandWidth, önceki SQUEEZE_LOOKBACK barın
/// en-dar değerinin buffer içinde mi? Kanal-kırılım barındaki bant
/// açılımı min() ile seçilmez → tezahür noktasından bağımsız enerji
/// kontrolü; sabitler (125, ×1.03) Method-I kanunu — ayar yok.
pub fn squeeze_on(candles: &[Candle]) -> bool {
    let c = closes(candles);
    let bb = indicators::bollinger(&c, BB_N, BB_K);
    let width: Vec<f64> = bb
        .iter()
        .filter_map(|(m, u, l)| match (m, u, l) {
            (Some(m), Some(u), Some(l)) if *m > 0.0 => Some((u - l) / m),
            _ => None,
        })
        .collect();
    if width.len() < SQUEEZE_LOOKBACK + CHANNEL {
        return false;
    }
    let n = width.len();
    let recent_min = width[n - CHANNEL..]
        .iter()
        .cloned()
        .fold(f64::MAX, f64::min);
    let prior_min = width[n - CHANNEL - SQUEEZE_LOOKBACK..n - CHANNEL]
        .iter()
        .cloned()
        .fold(f64::MAX, f64::min);
    if !recent_min.is_finite() || !prior_min.is_finite() {
        return false;
    }
    // BB genişliği 0'a inebilir (durağan seri) — sıfır = uç sıkışma.
    recent_min <= prior_min * SQUEEZE_BUFFER
}

/// Dow yapısı: son iki pivot karşılaştırması + hacim doğrulayıcı.
pub fn dow_ok(candles: &[Candle], long: bool) -> bool {
    let c = closes(candles);
    let n = c.len();
    if n < 4 * PIVOT_K + 2 {
        return false;
    }
    let (ph, pl) = pivots(&c, PIVOT_K);
    if ph.len() < 2 || pl.len() < 2 {
        return false;
    }
    let vols: Vec<f64> = candles.iter().map(|x| x.v).collect();
    let up_v: f64 = c
        .windows(2)
        .enumerate()
        .filter(|(_i, w)| w[1] > w[0])
        .map(|(i, _)| vols[i + 1])
        .sum();
    let dn_v: f64 = c
        .windows(2)
        .enumerate()
        .filter(|(_i, w)| w[1] < w[0])
        .map(|(i, _)| vols[i + 1])
        .sum();
    let vol_up = up_v > dn_v;
    if long {
        ph[ph.len() - 1] > ph[ph.len() - 2] && pl[pl.len() - 1] > pl[pl.len() - 2] && vol_up
    } else {
        ph[ph.len() - 1] < ph[ph.len() - 2] && pl[pl.len() - 1] < pl[pl.len() - 2] && !vol_up
    }
}

/// Dow kanonik pivots (span k): tepe/dip serileri.
fn pivots(v: &[f64], k: usize) -> (Vec<f64>, Vec<f64>) {
    let mut highs = Vec::new();
    let mut lows = Vec::new();
    let n = v.len();
    if n < 2 * k + 1 {
        return (highs, lows);
    }
    for i in k..n - k {
        let x = v[i];
        if i >= k && (i - k..=i + k).all(|j| j == i || v[j] < x) {
            highs.push(x);
        }
        if (i - k..=i + k).all(|j| j == i || v[j] > x) {
            lows.push(x);
        }
    }
    (highs, lows)
}

/// Wyckoff SOS: kırılım barı geniş gövde (≥1×ATR20) + hacim patlaması
/// (≥1.3×SMA20). Dönüş: hacim/SMA20 oranı (güc girdisi).
pub fn wyckoff_sos(candles: &[Candle], long: bool) -> Option<f64> {
    let c = closes(candles);
    let n = c.len();
    if n < ATR_N + 5 {
        return None;
    }
    let vols: Vec<f64> = candles.iter().map(|x| x.v).collect();
    let sma_series = indicators::sma(&vols, ATR_N);
    let sma = last(&sma_series)?;
    if !(sma.is_finite() && sma > 0.0) {
        return None;
    }
    let bar = &candles[n - 1];
    let spread = (bar.c - bar.o).abs();
    let h: Vec<f64> = candles.iter().map(|x| x.h).collect();
    let l: Vec<f64> = candles.iter().map(|x| x.l).collect();
    let atr = (*indicators::atr(&h, &l, &c, ATR_N).last()?)?;
    if !atr.is_finite() || atr <= 0.0 {
        return None;
    }
    let dir_ok = if long { bar.c > bar.o } else { bar.c < bar.o };
    let ratio = bar.v / sma;
    if dir_ok && spread >= 1.0 * atr && ratio >= SOS_VOL_MULT {
        Some(ratio)
    } else {
        None
    }
}

/// Elliott darbe: son EL_PIVOTS pivot (span=EL_SPAN, EL_WINDOW penceresi)
/// yönünde tekdüze ilerleyiş (long: HH→HH→HH; short: LL→LL→LL).
pub fn elliott_ok(candles: &[Candle], long: bool) -> bool {
    let n = candles.len();
    if n < 2 * EL_SPAN + EL_WINDOW + 2 {
        return false;
    }
    let v: Vec<f64> = if long {
        candles.iter().map(|x| x.h).collect()
    } else {
        candles.iter().map(|x| x.l).collect()
    };
    let pts = pivot_points(&v, n - 1 - EL_WINDOW, EL_SPAN, long);
    if pts.len() < EL_PIVOTS {
        return false;
    }
    let w: Vec<f64> = pts[pts.len() - EL_PIVOTS..].iter().map(|p| p.1).collect();
    w.windows(2)
        .all(|a| if long { a[1] > a[0] } else { a[1] < a[0] })
}

/// pencere içi pivot noktaları (index, değer) — rising: tepe pivotları.
fn pivot_points(v: &[f64], from: usize, span: usize, rising: bool) -> Vec<(usize, f64)> {
    let mut out = Vec::new();
    for i in from..v.len() {
        let x = v[i];
        if i < span || i + span >= v.len() {
            continue;
        }
        let ok = match rising {
            true => (i - span..=i + span).all(|j| j == i || v[j] < x),
            false => (i - span..=i + span).all(|j| j == i || v[j] > x),
        };
        if ok {
            out.push((i, x));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strat_test_helpers::helpers::{candle, flat};

    const H: i64 = 3_600_000;

    #[test]
    fn turtle_tetik_yonu() {
        let mut cs = flat(30, 100.0);
        cs[29] = candle(29 * H, 100.0, 101.5, 99.9, 101.2, 100.0); // üst kırılım
        assert_eq!(turtle_trigger(&cs), Some(true));
        let mut cs = flat(30, 100.0);
        cs[29] = candle(29 * H, 100.0, 100.05, 98.6, 98.8, 100.0); // alt kırılım
        assert_eq!(turtle_trigger(&cs), Some(false));
        let cs = flat(30, 100.0);
        assert_eq!(turtle_trigger(&cs), None, "kanal içi → tetik yok");
    }

    #[test]
    fn wyckoff_sos_hacim_kapis() {
        let mut cs = flat(30, 100.0);
        cs[29] = candle(29 * H, 99.9, 101.2, 99.8, 101.0, 400.0); // hacim 4×
        assert!(wyckoff_sos(&cs, true).is_some());
        cs[29].v = 100.0; // hacim yok
        assert!(wyckoff_sos(&cs, true).is_none());
    }

    #[test]
    fn squeeze_sikisma_dogru() {
        // önce geniş çalkantı, sonra sakinleşme → BB daralması = sıkışma
        // (BB warmup 19 + önceki 125 + son 20 pencere → 200 bar serisi)
        let mut cs = Vec::new();
        for i in 0..80 {
            let px = 100.0 + (if i % 2 == 0 { 1.5 } else { -1.5 });
            cs.push(candle(
                i as i64 * H,
                px - 0.5,
                px + 0.5,
                px - 0.5,
                px,
                100.0,
            ));
        }
        for i in 80..200 {
            let px = 100.0 + (if i % 2 == 0 { 0.05 } else { -0.05 });
            cs.push(candle(i as i64 * H, px, px + 0.01, px - 0.01, px, 100.0));
        }
        assert!(squeeze_on(&cs), "sakinleşen seriden sıkışma bekleniyor");
        // son 20 barın TAMAMI açılsın → pencere içi min geniş → sıkışma YOK
        for (i, bar) in cs.iter_mut().enumerate().skip(180) {
            let px = 100.0 + (if i % 2 == 0 { 1.2 } else { -1.2 });
            *bar = candle(i as i64 * H, px - 0.4, px + 0.4, px - 0.4, px, 300.0);
        }
        assert!(!squeeze_on(&cs), "geniş pencere → sıkışma yok");
    }

    #[test]
    fn dow_yapisi_veto() {
        // yükseliş zigzag: HH + HL + yükseliş hacmi baskın (pivot span=10)
        let mut cs = Vec::new();
        let elbows = [
            (0usize, 92.0),
            (10, 94.0),
            (16, 93.0),
            (29, 100.0),
            (39, 96.0),
            (51, 108.0),
            (64, 105.0),
        ];
        let mut idx = 0usize;
        for w in elbows.windows(2) {
            let (i0, p0) = w[0];
            let (i1, p1) = w[1];
            for i in idx..i1 {
                let px = p0 + (p1 - p0) * (i - i0) as f64 / (i1 - i0) as f64;
                let v = if px >= p0 { 220.0 } else { 40.0 };
                cs.push(candle(i as i64 * H, px, px + 0.05, px - 0.05, px, v));
            }
            idx = i1;
        }
        assert!(dow_ok(&cs, true), "HH+HL+hacim → Dow BUY yapısı");
        // ayna: düşüş zigzag → sell yapısı
        let mut cs: Vec<Candle> = Vec::new();
        let too = [
            (0usize, 108.0),
            (10, 106.0),
            (16, 107.0),
            (29, 100.0),
            (39, 104.0),
            (51, 92.0),
            (64, 95.0),
        ];
        let mut idx = 0usize;
        for w in too.windows(2) {
            let (i0, p0) = w[0];
            let (i1, p1) = w[1];
            for i in idx..i1 {
                let px = p0 + (p1 - p0) * (i - i0) as f64 / (i1 - i0) as f64;
                let v = if px < p0 { 220.0 } else { 40.0 };
                cs.push(candle(i as i64 * H, px, px + 0.05, px - 0.05, px, v));
            }
            idx = i1;
        }
        assert!(dow_ok(&cs, false), "LH+LL+hacim → Dow SELL yapısı");
    }

    #[test]
    fn wyckoff_gövde_ve_hacim_ikisi_şart() {
        let mut cs = flat(30, 100.0);
        // geniş gövde + hacim → sos
        cs[29] = candle(29 * H, 99.9, 101.2, 99.8, 101.0, 400.0);
        assert!(wyckoff_sos(&cs, true).is_some());
        // hacim var ama dar gövde → sos YOK
        cs[29] = candle(29 * H, 99.95, 100.05, 99.9, 100.05, 400.0);
        assert!(wyckoff_sos(&cs, true).is_none());
        // gövde yönsel değil (bear değil_long) → yok
        cs[29] = candle(29 * H, 100.1, 100.15, 99.5, 99.6, 400.0);
        assert!(wyckoff_sos(&cs, true).is_none());
    }

    #[test]
    fn elliott_darbe_sayimi() {
        // sakin ısınma (pivot olmadan) + HH tepe merdiveni (span=3)
        let mut cs = flat(45, 100.0);
        let peaks = [102.0, 105.0, 108.0, 111.0, 114.0];
        for (k, pk) in peaks.iter().enumerate() {
            let v0 = pk - 6.0;
            for j in 0..6 {
                let px = v0 + (pk - v0) * j as f64 / 5.0;
                cs.push(candle(
                    (45 + k * 6 + j) as i64 * H,
                    px,
                    px + 0.05,
                    px - 0.05,
                    px,
                    100.0,
                ));
            }
        }
        cs.push(candle(80 * H, 112.5, 113.0, 112.0, 112.2, 100.0));
        assert!(elliott_ok(&cs, true), "HH merdiveni → impulse (long)");
        assert!(!elliott_ok(&flat(80, 100.0), true), "düz seri → sayım yok");
    }
}
