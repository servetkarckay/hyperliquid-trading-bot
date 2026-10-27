//! SWEEP + CISD (ICT / Smart Money Concepts) — likidite avı ve teslimat
//! değişimi.
//!
//! **Long**: swing dip / önceki gün dip (PDL) altına fitil atılır (sweep),
//! ardından hacimli yukarı displacement bir FVG bırakır, son düşüş
//! mumunun açılışının üstünde gövde kapanışı gelir (CISD) ve fiyat
//! OB/FVG bölgesine geri çekilip reddeder → giriş sinyali.
//!
//! **Short**: ayna görüntüsü (tepe likiditesi sweep → aşağı displacement
//! + FVG → son yükseliş mumunun açılışı altında kapanış → pullback).
//!
//! Filtreler:
//! - **HTF uyumu**: 1h mumlardan türetilen 4h EMA20 yönü (karşı-trend
//!   kurulum sinyal üretmez),
//! - **Fake CISD koruması**: displacement gövdesi ≥ `0.7×ATR` ve mutlaka
//!   FVG (imbalance) bırakmalı,
//! - **CISD**: onay kapanışı gövde kapanışı olmalı (fitil yetmez),
//! - **Sweep**: kapanış seviyenin geri içinde + fitil bar aralığının
//!   ≥ %50'si.
//!
//! Deterministik, replay-idempotent, panik yok; yetersiz veri → `Skip`.
use crate::closes;
use data::Candle;
use math::indicators;

pub struct SweepCisd;

/// 1h mum: 120-bar likidite penceresi + 4h EMA20 (≈84 bar) ısınması.
pub const MIN_BARS: usize = 220;
const ATR_N: usize = 14;
const SWING_SPAN: usize = 2;
const SWING_LOOKBACK: usize = 120;
const SWEEP_LOOKBACK: usize = 12;
const WICK_MIN: f64 = 0.5;
const DISP_BODY_ATR: f64 = 0.7;
const MIN_RR: f64 = 1.5;
const SL_BUFFER_ATR: f64 = 0.1;
const HTF_EMA_N: usize = 20;
const HTF_MS: i64 = 4 * 3_600_000;
const DAY_BARS: usize = 24;

/// Sinyalin fiyat planı (SL/TP dahil) — test ve doğrulama için açık.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub long: bool,
    pub entry: f64,
    pub sl: f64,
    pub tp: f64,
    pub strength: f64,
    /// sweep barının likidite seviyesi (fitilin kırdığı seviye)
    pub sweep_level: f64,
    /// sweep fitilinin ucu (SL referansı)
    pub sweep_extreme: f64,
    pub cisd_level: f64,
    /// OB gövdesi ∪ FVG giriş bölgesi (alt, üst)
    pub zone: (f64, f64),
}

struct Setup {
    confirm: usize,
    disp_body: f64,
    wick_ratio: f64,
    cisd: f64,
    level: f64,
    extreme: f64,
    zone: (f64, f64),
    fvg: (f64, f64),
}

impl SweepCisd {
    /// Kurulum taraması — sinyal varsa fiyat planıyla döner.
    pub fn plan(candles: &[Candle]) -> Option<Plan> {
        if candles.len() < MIN_BARS {
            return None;
        }
        let h: Vec<f64> = candles.iter().map(|c| c.h).collect();
        let l: Vec<f64> = candles.iter().map(|c| c.l).collect();
        let c = closes(candles);
        let atr = (*indicators::atr(&h, &l, &c, ATR_N).last()?)?;
        if !atr.is_finite() || atr <= 0.0 {
            return None;
        }
        let last = candles.len() - 1;
        if htf_trend_up(candles)? {
            plan_long(candles, &h, &l, atr, last)
        } else {
            plan_short(candles, &h, &l, atr, last)
        }
    }
}

fn plan_long(candles: &[Candle], h: &[f64], l: &[f64], atr: f64, last: usize) -> Option<Plan> {
    let start = last.saturating_sub(SWEEP_LOOKBACK);
    let (sweep, st) = (start..last)
        .rev()
        .find_map(|s| long_setup(candles, l, atr, s, last).map(|st| (s, st)))?;
    let bar = &candles[last];
    let touched = if st.confirm == last {
        true
    } else {
        bar.c > bar.o && bar.l <= st.zone.1 && bar.c >= st.zone.0
    };
    if !touched || bar.c <= st.cisd {
        return None;
    }
    let entry = bar.c;
    let sl = candles[sweep].l - SL_BUFFER_ATR * atr;
    let risk = entry - sl;
    if risk.is_nan() || risk <= 0.0 {
        return None;
    }
    let tp = target_long(h, last, entry, risk);
    if (tp - entry) / risk < MIN_RR {
        return None;
    }
    Some(build_plan(true, entry, sl, tp, atr, st))
}

fn plan_short(candles: &[Candle], h: &[f64], l: &[f64], atr: f64, last: usize) -> Option<Plan> {
    let start = last.saturating_sub(SWEEP_LOOKBACK);
    let (sweep, st) = (start..last)
        .rev()
        .find_map(|s| short_setup(candles, h, atr, s, last).map(|st| (s, st)))?;
    let bar = &candles[last];
    let touched = if st.confirm == last {
        true
    } else {
        bar.c < bar.o && bar.h >= st.zone.0 && bar.c <= st.zone.1
    };
    if !touched || bar.c >= st.cisd {
        return None;
    }
    let entry = bar.c;
    let sl = candles[sweep].h + SL_BUFFER_ATR * atr;
    let risk = sl - entry;
    if risk.is_nan() || risk <= 0.0 {
        return None;
    }
    let tp = target_short(l, last, entry, risk);
    if (entry - tp) / risk < MIN_RR {
        return None;
    }
    Some(build_plan(false, entry, sl, tp, atr, st))
}

fn build_plan(long: bool, entry: f64, sl: f64, tp: f64, atr: f64, st: Setup) -> Plan {
    // Güç: displacement büyüklüğü + FVG genişliği + fitil baskınlığı.
    let d = (st.disp_body / atr).clamp(0.0, 2.0) / 2.0;
    let f = ((st.fvg.1 - st.fvg.0) / atr).clamp(0.0, 1.0);
    let w = st.wick_ratio.clamp(0.0, 1.0);
    let strength = (0.35 + 0.25 * d + 0.20 * f + 0.15 * w).clamp(0.35, 1.0);
    Plan {
        long,
        entry,
        sl,
        tp,
        strength,
        sweep_level: st.level,
        sweep_extreme: st.extreme,
        cisd_level: st.cisd,
        zone: st.zone,
    }
}

/// Long sweep: dip likiditesi alınır, kapanış seviyenin üstünde, fitil baskın.
fn long_setup(candles: &[Candle], l: &[f64], atr: f64, s: usize, last: usize) -> Option<Setup> {
    let cd = &candles[s];
    let range = cd.h - cd.l;
    if range.is_nan() || range <= 0.0 {
        return None;
    }
    let lower_wick = cd.o.min(cd.c) - cd.l;
    let wick_ratio = lower_wick / range;
    if wick_ratio < WICK_MIN {
        return None;
    }
    let level = liquidity_lows(l, s)
        .into_iter()
        .filter(|lv| cd.l < *lv && cd.c > *lv)
        .fold(f64::NEG_INFINITY, f64::max);
    if !level.is_finite() {
        return None;
    }
    // Displacement + FVG (3 mumluk imbalance, orta mum yönlü ve güçlü).
    let (m, fvg) = first_bull_fvg(candles, s, atr)?;
    // CISD: displacement öncesi SON düşüş mumunun açılışının üstünde
    // gövde kapanışı.
    let ob = (s..m).rev().find(|&i| candles[i].c < candles[i].o)?;
    let cisd = candles[ob].o;
    let confirm = (ob + 1..=last).find(|&i| candles[i].c > cisd && candles[i].c > candles[i].o)?;
    // Geçersizleşme: sweep dibinin altında kapanış yok.
    if (s + 1..=last).any(|i| candles[i].c < candles[s].l) {
        return None;
    }
    let ob_lo = candles[ob].o.min(candles[ob].c);
    let ob_hi = candles[ob].o.max(candles[ob].c);
    let zone = (ob_lo.min(fvg.0), ob_hi.max(fvg.1));
    Some(Setup {
        confirm,
        disp_body: (candles[m].c - candles[m].o).abs(),
        wick_ratio,
        cisd,
        level,
        extreme: cd.l,
        zone,
        fvg,
    })
}

/// Short sweep: tepe likiditesi alınır, kapanış seviyenin altında, fitil baskın.
fn short_setup(candles: &[Candle], h: &[f64], atr: f64, s: usize, last: usize) -> Option<Setup> {
    let cd = &candles[s];
    let range = cd.h - cd.l;
    if range.is_nan() || range <= 0.0 {
        return None;
    }
    let upper_wick = cd.h - cd.o.max(cd.c);
    let wick_ratio = upper_wick / range;
    if wick_ratio < WICK_MIN {
        return None;
    }
    let level = liquidity_highs(h, s)
        .into_iter()
        .filter(|lv| cd.h > *lv && cd.c < *lv)
        .fold(f64::INFINITY, f64::min);
    if !level.is_finite() {
        return None;
    }
    let (m, fvg) = first_bear_fvg(candles, s, atr)?;
    let ob = (s..m).rev().find(|&i| candles[i].c > candles[i].o)?;
    let cisd = candles[ob].o;
    let confirm = (ob + 1..=last).find(|&i| candles[i].c < cisd && candles[i].c < candles[i].o)?;
    if (s + 1..=last).any(|i| candles[i].c > candles[s].h) {
        return None;
    }
    let ob_lo = candles[ob].o.min(candles[ob].c);
    let ob_hi = candles[ob].o.max(candles[ob].c);
    let zone = (ob_lo.min(fvg.0), ob_hi.max(fvg.1));
    Some(Setup {
        confirm,
        disp_body: (candles[m].c - candles[m].o).abs(),
        wick_ratio,
        cisd,
        level,
        extreme: cd.h,
        zone,
        fvg,
    })
}

/// (orta, (fvg_alt, fvg_üst)) — ilk geçerli boğa FVG'si.
/// `from` = sweep barı; FVG sweep'ten SONRA oluşmalı.
fn first_bull_fvg(candles: &[Candle], from: usize, atr: f64) -> Option<(usize, (f64, f64))> {
    (from + 2..candles.len()).find_map(|j| {
        let c0 = &candles[j - 2];
        let m = &candles[j - 1];
        let c2 = &candles[j];
        (c2.l > c0.h && m.c > m.o && (m.c - m.o) >= DISP_BODY_ATR * atr)
            .then_some((j - 1, (c0.h, c2.l)))
    })
}

/// (orta, (fvg_alt, fvg_üst)) — ilk geçerli ayı FVG'si.
fn first_bear_fvg(candles: &[Candle], from: usize, atr: f64) -> Option<(usize, (f64, f64))> {
    (from + 2..candles.len()).find_map(|j| {
        let c0 = &candles[j - 2];
        let m = &candles[j - 1];
        let c2 = &candles[j];
        (c2.h < c0.l && m.c < m.o && (m.o - m.c) >= DISP_BODY_ATR * atr)
            .then_some((j - 1, (c2.h, c0.l)))
    })
}

/// `s` barından ÖNCE oluşmuş dip likidite seviyeleri:
/// swing dip pivotları + önceki gün dibi (24 bar).
fn liquidity_lows(l: &[f64], s: usize) -> Vec<f64> {
    let mut out = Vec::new();
    let lo = s.saturating_sub(SWING_LOOKBACK);
    for i in lo..s {
        if i >= SWING_SPAN && i + SWING_SPAN < s && is_pivot_low(l, i, SWING_SPAN) {
            out.push(l[i]);
        }
    }
    if s >= DAY_BARS {
        out.push(
            l[s - DAY_BARS..s]
                .iter()
                .cloned()
                .fold(f64::INFINITY, f64::min),
        );
    }
    out
}

/// `s` barından ÖNCE oluşmuş tepe likidite seviyeleri.
fn liquidity_highs(h: &[f64], s: usize) -> Vec<f64> {
    let mut out = Vec::new();
    let lo = s.saturating_sub(SWING_LOOKBACK);
    for i in lo..s {
        if i >= SWING_SPAN && i + SWING_SPAN < s && is_pivot_high(h, i, SWING_SPAN) {
            out.push(h[i]);
        }
    }
    if s >= DAY_BARS {
        out.push(
            h[s - DAY_BARS..s]
                .iter()
                .cloned()
                .fold(f64::NEG_INFINITY, f64::max),
        );
    }
    out
}

fn is_pivot_low(v: &[f64], i: usize, span: usize) -> bool {
    let x = v[i];
    (i - span..=i + span).all(|k| k == i || v[k] > x)
}

fn is_pivot_high(v: &[f64], i: usize, span: usize) -> bool {
    let x = v[i];
    (i - span..=i + span).all(|k| k == i || v[k] < x)
}

/// En yakın (≥ MIN_RR veren) üst likidite havuzu; yoksa 3R yedek hedef.
fn target_long(h: &[f64], last: usize, entry: f64, risk: f64) -> f64 {
    let lo = last.saturating_sub(SWING_LOOKBACK);
    let mut levels: Vec<f64> = (lo..=last)
        .filter(|&i| i >= SWING_SPAN && i + SWING_SPAN <= last && is_pivot_high(h, i, SWING_SPAN))
        .map(|i| h[i])
        .collect();
    if last >= DAY_BARS {
        levels.push(
            h[last - DAY_BARS..last]
                .iter()
                .cloned()
                .fold(f64::NEG_INFINITY, f64::max),
        );
    }
    levels.retain(|x| x.is_finite() && *x > entry);
    levels.sort_by(f64::total_cmp);
    levels
        .into_iter()
        .find(|x| (x - entry) / risk >= MIN_RR)
        .unwrap_or(entry + 3.0 * risk)
}

/// En yakın (≥ MIN_RR veren) alt likidite havuzu; yoksa 3R yedek hedef.
fn target_short(l: &[f64], last: usize, entry: f64, risk: f64) -> f64 {
    let lo = last.saturating_sub(SWING_LOOKBACK);
    let mut levels: Vec<f64> = (lo..=last)
        .filter(|&i| i >= SWING_SPAN && i + SWING_SPAN <= last && is_pivot_low(l, i, SWING_SPAN))
        .map(|i| l[i])
        .collect();
    if last >= DAY_BARS {
        levels.push(
            l[last - DAY_BARS..last]
                .iter()
                .cloned()
                .fold(f64::INFINITY, f64::min),
        );
    }
    levels.retain(|x| x.is_finite() && *x < entry);
    levels.sort_by(|a, b| b.total_cmp(a));
    levels
        .into_iter()
        .find(|x| (entry - x) / risk >= MIN_RR)
        .unwrap_or(entry - 3.0 * risk)
}

/// 4h kapanışları (son tamamlanmamış bucket atılır).
fn htf_closes(candles: &[Candle]) -> Vec<f64> {
    let mut buckets: Vec<(i64, f64)> = Vec::new();
    for cd in candles {
        let b = cd.t.div_euclid(HTF_MS);
        match buckets.last_mut() {
            Some((bb, c)) if *bb == b => *c = cd.c,
            _ => buckets.push((b, cd.c)),
        }
    }
    if buckets.len() > 1 {
        buckets.pop();
    }
    buckets.into_iter().map(|(_, c)| c).collect()
}

/// 4h EMA20 yönü: kapanış EMA üstünde → yukarı; altında → aşağı.
fn htf_trend_up(candles: &[Candle]) -> Option<bool> {
    let hc = htf_closes(candles);
    if hc.len() < HTF_EMA_N + 2 {
        return None;
    }
    let e = indicators::ema(&hc, HTF_EMA_N);
    let ema = (*e.last()?)?;
    let ema_prev = e[e.len() - 2]?;
    // Yön EMA ANAHTARINDAN değil: kapanış tarafı + EMA EĞİMİ şart.
    let up = hc[hc.len() - 1] > ema && ema >= ema_prev;
    let down = hc[hc.len() - 1] < ema && ema <= ema_prev;
    if up {
        Some(true)
    } else if down {
        Some(false)
    } else {
        None // yatay/çorap rejimi → kurulum yok (faz/agresif değil)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strat_test_helpers::helpers::{candle, flat};
    use crate::{highs, lows};

    const H: i64 = 3_600_000;

    fn long_series() -> Vec<Candle> {
        let mut cs = flat(221, 100.0);
        // 211: üst likidite (hedef havuz)
        cs[211] = candle(211 * H, 100.5, 104.5, 100.4, 100.6, 100.0);
        // 214: swing dip likiditesi 99.5
        cs[214] = candle(214 * H, 100.0, 100.05, 99.5, 99.8, 100.0);
        // 217: sweep — PDL(99.5) altına fitil, kapanış geri içinde
        cs[217] = candle(217 * H, 99.9, 100.0, 99.0, 99.7, 200.0);
        // 218: boğa displacement (gövde ≈ 1.1 > 0.7×ATR)
        cs[218] = candle(218 * H, 99.7, 101.0, 99.6, 100.8, 300.0);
        // 219: FVG üçüncü barı (low 101.0 > 217.high 100.0)
        cs[219] = candle(219 * H, 100.8, 101.6, 101.0, 101.3, 200.0);
        // 220: OB/FVG bölgesine pullback + boğa reddi (CISD üstünde)
        cs[220] = candle(220 * H, 100.9, 101.2, 100.2, 101.1, 150.0);
        cs
    }

    fn short_series() -> Vec<Candle> {
        let mut cs = flat(221, 100.0);
        // 211: alt likidite (hedef havuz)
        cs[211] = candle(211 * H, 99.5, 99.6, 95.5, 99.4, 100.0);
        // 214: swing tepe likiditesi 100.5
        cs[214] = candle(214 * H, 100.0, 100.5, 99.95, 100.2, 100.0);
        // 217: sweep — tepe üstüne fitil, kapanış geri içinde
        cs[217] = candle(217 * H, 100.1, 101.0, 100.0, 100.3, 200.0);
        // 218: ayı displacement
        cs[218] = candle(218 * H, 100.3, 100.4, 99.0, 99.2, 300.0);
        // 219: FVG üçüncü barı (high 99.0 < 217.low 100.0)
        cs[219] = candle(219 * H, 99.0, 99.0, 98.4, 98.6, 200.0);
        // 220: bölgeye pullback + ayı reddi (CISD altında)
        cs[220] = candle(220 * H, 99.9, 99.9, 98.9, 99.4, 150.0);
        cs
    }

    #[test]
    fn uzun_kurulum_sinyal_uretir() {
        let plan = SweepCisd::plan(&long_series()).expect("long plan");
        assert!(plan.long);
        assert!((plan.entry - 101.1).abs() < 1e-9);
        assert!(plan.sl < 99.0 && plan.sl > 98.8, "sl={}", plan.sl);
        assert!((plan.tp - 104.5).abs() < 1e-9, "tp={}", plan.tp);
        assert!((plan.cisd_level - 99.9).abs() < 1e-9);
        assert!(plan.strength >= 0.35 && plan.strength <= 1.0);
        assert!(plan.zone.0 < plan.zone.1);
    }

    #[test]
    fn kisa_kurulum_sinyal_uretir() {
        let plan = SweepCisd::plan(&short_series()).expect("short plan");
        assert!(!plan.long);
        assert!((plan.entry - 99.4).abs() < 1e-9);
        assert!((plan.sl - 101.0).abs() < 0.1, "sl={}", plan.sl);
        assert!((plan.tp - 95.5).abs() < 1e-9, "tp={}", plan.tp);
    }

    #[test]
    fn fvg_yoksa_sinyal_yok() {
        let mut cs = long_series();
        // FVG kapat: üçüncü barın dibi sweep barının tepesinin altında
        cs[219] = candle(219 * H, 100.6, 101.2, 100.0, 101.0, 200.0);
        cs[220] = candle(220 * H, 100.7, 101.1, 100.1, 100.9, 150.0);
        assert!(SweepCisd::plan(&cs).is_none());
    }

    #[test]
    fn cisd_onayi_yoksa_sinyal_yok() {
        let mut cs = flat(221, 100.0);
        cs[214] = candle(214 * H, 100.0, 100.05, 99.5, 99.8, 100.0);
        // CISD seviyesi 101.0 — hiçbir kapanış üstüne çıkmaz
        cs[217] = candle(217 * H, 101.0, 101.0, 99.0, 100.6, 200.0);
        cs[218] = candle(218 * H, 100.6, 101.0, 100.4, 100.8, 300.0);
        cs[219] = candle(219 * H, 100.8, 101.6, 101.0, 101.3, 200.0);
        cs[220] = candle(220 * H, 100.7, 100.95, 100.2, 100.9, 150.0);
        assert!(SweepCisd::plan(&cs).is_none());
    }

    #[test]
    fn zayif_displacement_sinyal_uretmez() {
        let mut cs = long_series();
        // Gövde 0.1 < 0.7×ATR ve FVG yok
        cs[218] = candle(218 * H, 99.7, 100.0, 99.6, 99.8, 50.0);
        cs[219] = candle(219 * H, 99.8, 100.1, 99.7, 99.9, 50.0);
        cs[220] = candle(220 * H, 99.9, 100.2, 99.8, 100.0, 50.0);
        assert!(SweepCisd::plan(&cs).is_none());
    }

    #[test]
    fn karsi_trend_htf_long_engellenir() {
        // 4h kapanışları düşüşte → EMA altında → long kurulum reddedilir
        let mut cs = flat(221, 100.0);
        for (i, cd) in cs.iter_mut().enumerate() {
            let px = 120.0 - 0.1 * i as f64;
            *cd = candle(cd.t, px, px * 1.001, px * 0.999, px, 100.0);
        }
        cs[211] = candle(211 * H, 120.0, 104.0, 110.0, 111.0, 100.0);
        let h = highs(&cs);
        let l = lows(&cs);
        assert_eq!(htf_trend_up(&cs), Some(false), "htf düşüş bekleniyor");
        assert!(plan_long(&cs, &h, &l, 0.5, cs.len() - 1).is_none());
    }

    #[test]
    fn htf_yonu_dogru_hesaplanir() {
        let up: Vec<Candle> = (0..221)
            .map(|i| {
                let px = 100.0 + 0.1 * i as f64;
                candle(i as i64 * H, px, px * 1.001, px * 0.999, px, 100.0)
            })
            .collect();
        assert_eq!(htf_trend_up(&up), Some(true));
        let down: Vec<Candle> = (0..221)
            .map(|i| {
                let px = 120.0 - 0.1 * i as f64;
                candle(i as i64 * H, px, px * 1.001, px * 0.999, px, 100.0)
            })
            .collect();
        assert_eq!(htf_trend_up(&down), Some(false));
    }

    #[test]
    fn yetersiz_veri_kurulum_yok() {
        let cs = flat(MIN_BARS - 1, 100.0);
        assert!(SweepCisd::plan(&cs).is_none());
    }

    #[test]
    fn deterministik_ayni_girdi_ayni_plan() {
        let a = SweepCisd::plan(&long_series());
        let b = SweepCisd::plan(&long_series());
        assert_eq!(a, b);
    }
}
