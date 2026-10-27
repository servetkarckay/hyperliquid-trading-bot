//! SMC-L MODÜLÜ — Sweep+CISD + OFI + OI + Rejim tek sinyalde.
//!
//! Katman düzeni (veri varlığı → çekirdek → rejim → OFI → OI):
//! 1. **Çekirdek ICT kurulumu** (`SweepCisd::plan`, bu modüle birleşti):
//!    likidite sweep fitili
//!    → displacement + FVG → CISD gövde kapanışı → OB/FVG pullback.
//! 2. **Rejim** (`regime_detection::classify`): yalnız TREND rejiminde
//!    (ADX>25 + Hurst>0.55) trend yönündeki kurulumlar; VolSpike/Squeeze
//!    rejiminde giriş YOK (Blocked), MR/GEÇİŞ'te sinyal yok
//!    (SMC-L trend-düzeltme sinyalidir).
//! 3. **OFI onayı** (Cont-Kukanov–Stoikov ruhu): displacement arkasında
//!    agresif akış olmalı — imb ±0.15 + taker %55/%45 ikili onayı.
//! 4. **OI onayı** (Price×OI): sweep anında ΔOI < 0 (stop patlaması =
//!    likidite avı) ve görece eşik |ΔOI|/OI ≥ %0.2. OI artışı + fiyat
//!    sweep'i = yeni pozisyon itmesi, stop avı değil → fake koruması.
//! 5. **L2 defter onayı** (Market Making birleşimi, Avellaneda &
//!    Stoikov): borsa spread'i AS optimal tam-spread (2δ) içinde —
//!    kote edilebilir derinlik; sahte kırılımda defter boşalır.
//!
//! Veri yoksa `Skip` (sahte sinyal yok), katman ihlali `Hold` (bilinçli
//! bekleme). Deterministik, replay-idempotent, panik yok.
use crate::closes;
use crate::regime_detection::{classify, RegimeLabel};
use crate::{MarketInput, Signal, Strategy};
use data::Candle;
use math::indicators;

pub struct SmcL;

const IMB_MIN: f64 = 0.15;
const TAKER_BUY: f64 = 0.55;
const TAKER_SELL: f64 = 0.45;
const REL_OI_MIN: f64 = 0.002;

/// OI katmanı sonucu (test düzeyinde ayrıştırılabilir).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OiVerdict {
    /// katmandan geç — dönüşünde görece ΔOI döner
    Ok(f64),
    /// gürültü: görece ΔOI eşik altı
    Noise,
    /// sweep'te OI artışı — stop avı değil (fake koruması)
    Rising,
}

fn oi_verdict(d_oi: f64, oi_level: f64) -> OiVerdict {
    let rel = d_oi.abs() / oi_level.max(1e-12);
    // NaN/non-finite → gürültü (oy yok)
    if !rel.is_finite() || rel <= REL_OI_MIN {
        return OiVerdict::Noise;
    }
    if d_oi > 0.0 {
        return OiVerdict::Rising;
    }
    OiVerdict::Ok(rel)
}

impl Strategy for SmcL {
    fn name(&self) -> &'static str {
        "SMC-L"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        if input.candles.len() < MIN_BARS {
            return Signal::Skip {
                reason: "SMC-L için 1h mum penceresi yetersiz".into(),
            };
        }
        // ── Veri varlığı (SAHTE sinyal yok → en dış kapı)
        let (Some(imb), Some(taker)) = (input.book_imbalance, input.taker_ratio) else {
            return Signal::Skip {
                reason: "defter dengesi veya taker akışı yok".into(),
            };
        };
        let Some(d_oi) = input.oi_delta else {
            return Signal::Skip {
                reason: "ΔOI yok (ctx serisi kısa)".into(),
            };
        };
        let Some(ctx) = &input.ctx else {
            return Signal::Skip {
                reason: "OI seviyesi yok — görece ölçek hesaplanamaz".into(),
            };
        };
        // ── 1) Çekirdek kurulum
        let Some(plan) = SweepCisd::plan(&input.candles) else {
            return Signal::Hold {
                note: "SMC-L: sweep/CISD çekirdek kurulumu yok".into(),
            };
        };
        // ── 2) Rejim kapısı: yalnız TREND, yön eşleşmesi şart
        let Some((label, det)) = classify(input) else {
            return Signal::Hold {
                note: "SMC-L: rejim hesaplanamaz (<200 mum / göstergeler hazır değil)".into(),
            };
        };
        if !regime_permits(label, plan.long) {
            return Signal::Hold {
                note: format!("SMC-L: rejim uygun değil ({det})"),
            };
        }
        // ── 3) OFI: displacement arkasında agresif akış
        if !ofi_ok(plan.long, imb, taker) {
            return Signal::Hold {
                note: format!(
                    "SMC-L: OFI onaysız (imb={imb:+.2} taker={taker:.2}) — fake CISD riski"
                ),
            };
        }
        // ── 4) L2 defter katmanı (Market Making birleşimi): kote-edilebilir
        //    defter — borsa spread'i A&S optimal tam-spread (2δ) içinde
        let Some((l2_pass, l2_margin)) = l2_book_ok(input) else {
            return Signal::Hold {
                note: "SMC-L: L2 defter sorgusu yapilamadı (defter yok/bozuk)".into(),
            };
        };
        if !l2_pass {
            return Signal::Hold {
                note: "SMC-L: L2 spread A&S optimal üstü — defter genişlendi (geçiş yok)".into(),
            };
        }
        // ── 5) OI: sweep = stop patlaması → ΔOI < 0, görece ≥ %0.2
        let rel_raw = d_oi.abs() / ctx.open_interest.max(1e-12);
        let rel = match oi_verdict(d_oi, ctx.open_interest) {
            OiVerdict::Ok(v) => v,
            OiVerdict::Noise => {
                return Signal::Hold {
                    note: format!("SMC-L: ΔOI gürültü (görece %{:.2})", rel_raw * 100.0),
                };
            }
            OiVerdict::Rising => {
                return Signal::Hold {
                    note: format!(
                        "SMC-L: sweep'te OI ARTIŞI (%{:.2}) — stop patlaması değil, fake koruması",
                        rel_raw * 100.0
                    ),
                };
            }
        };
        // ── Katman toplamı: çekirdek güç + taker yoğunluğu + ΔOI şiddeti
        let strength = (plan.strength
            + 0.10 * ((taker - 0.5).abs() * 4.0).min(1.0)
            + 0.05 * l2_margin
            + 0.10 * (rel.clamp(0.0, 0.02) / 0.02))
            .min(1.0);
        let note = format!(
            "SMC-L katmanları ✓ · sweep {:.6} CISD {:.6} · imb {imb:+.2} taker %{:.0} · ΔOI −%{:.2} · SL {:.6} TP {:.6}",
            plan.sweep_level,
            plan.cisd_level,
            taker * 100.0,
            rel * 100.0,
            plan.sl,
            plan.tp
        );
        if plan.long {
            Signal::Long { strength, note }
        } else {
            Signal::Short { strength, note }
        }
    }
}

fn regime_permits(label: RegimeLabel, long: bool) -> bool {
    matches!(label, RegimeLabel::Trend { up } if up == long)
}

/// L2 defter katmanı — Market Making (Avellaneda & Stoikov, 2008)
/// mirası: borsa spread'i (ask−bid), AS optimal yarı-spread δ'nin
/// 2 katından küçük-олmalı (kote edilebilir/derin defter):
/// δ = γσ²(T−t)/2 + (1/γ)·ln(1+γ/κ), γ=0.1, κ=1.5, T=1 bar,
/// σ = 50-bar realized (makale §3.1). Dönüş: (pass, strength-marjı).
fn l2_book_ok(input: &crate::MarketInput) -> Option<(bool, f64)> {
    let (Some(bid), Some(ask)) = (input.best_bid, input.best_ask) else {
        return None;
    };
    if !(bid < ask && ask > 0.0 && bid > 0.0) {
        return None;
    }
    let c = closes(&input.candles);
    if c.len() < 60 {
        return None;
    }
    let r = indicators::log_returns(&c);
    if r.len() < 51 {
        return None;
    }
    let w = &r[r.len() - 50..];
    let m = w.iter().sum::<f64>() / w.len() as f64;
    let sigma = (w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (w.len() - 1) as f64).sqrt();
    if !(sigma.is_finite() && sigma > 0.0) {
        return None;
    }
    let gamma = 0.1;
    let kappa = 1.5;
    let half = gamma * sigma * sigma / 2.0 + (1.0 + gamma / kappa).ln() / gamma;
    if !(half.is_finite() && half > 0.0) {
        return None;
    }
    let full = 2.0 * half;
    let spread = ask - bid;
    let margin = ((full - spread) / full).clamp(0.0, 1.0);
    Some((spread <= full, margin))
}

fn ofi_ok(long: bool, imb: f64, taker: f64) -> bool {
    if long {
        imb > IMB_MIN && taker > TAKER_BUY
    } else {
        imb < -IMB_MIN && taker < TAKER_SELL
    }
}

/// Test/rapor için çekirdek plan erişimi.
#[allow(dead_code)]
pub fn core_plan(candles: &[Candle]) -> Option<Plan> {
    SweepCisd::plan(candles)
}

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
mod ict_core_tests {
    use super::*;
    use crate::strat_test_helpers::helpers::{candle, flat};

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
        let h: Vec<f64> = cs.iter().map(|x| x.h).collect();
        let l: Vec<f64> = cs.iter().map(|x| x.l).collect();
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::strat_test_helpers::helpers::{candle, flat};
    use data::market_ctx::CtxState;

    const H: i64 = 3_600_000;

    /// Core plan onaylı kurulum (sweep_cisd testiyle birebir aynı şema).
    fn long_series() -> Vec<Candle> {
        let mut cs = flat(221, 100.0);
        cs[211] = candle(211 * H, 100.5, 104.5, 100.4, 100.6, 100.0);
        cs[214] = candle(214 * H, 100.0, 100.05, 99.5, 99.8, 100.0);
        cs[217] = candle(217 * H, 99.9, 100.0, 99.0, 99.7, 200.0);
        cs[218] = candle(218 * H, 99.7, 101.0, 99.6, 100.8, 300.0);
        cs[219] = candle(219 * H, 100.8, 101.6, 101.0, 101.3, 200.0);
        cs[220] = candle(220 * H, 100.9, 101.2, 100.2, 101.1, 150.0);
        cs
    }

    /// Tüm onay katmanları dolu, geçerli giriş senaryosu.
    fn onay_input(cs: Vec<Candle>) -> MarketInput {
        MarketInput {
            coin: "TEST".into(),
            candles: cs,
            ctx: Some(CtxState {
                coin: "TEST".into(),
                funding: 0.0,
                open_interest: 1_000_000.0,
                mark_px: 100.0,
                oracle_px: 100.0,
                premium: 0.0,
                day_ntl_vlm: 1_000_000.0,
                prev_day_px: 100.0,
                ts: 0,
            }),
            oi_delta: Some(-2_500.0),
            taker_ratio: Some(0.60),
            book_imbalance: Some(0.30),
            ..Default::default()
        }
    }

    /// End-to-end eşleşme: çekirdek kurulup OFI/OI onaylarına rağmen
    /// Rejim kapısı (TREND şartı) sinyal engeller → Hold. Katman sırası
    /// kuralını doğrular (Rejim > OFI > OI > çekirdek).
    #[test]
    fn rejim_uygunsuz_olsa_sinyal_yok() {
        let s = SmcL.evaluate(&onay_input(long_series()));
        assert!(
            matches!(s, Signal::Hold { .. }),
            "rejim katmanı kurulumu durdurmalı: {s:?}"
        );
    }

    #[test]
    fn ofi_ikili_esik() {
        assert!(ofi_ok(true, 0.30, 0.60));
        assert!(ofi_ok(false, -0.30, 0.40));
        assert!(!ofi_ok(true, 0.10, 0.60), "imb eşiği altı");
        assert!(!ofi_ok(true, 0.30, 0.50), "taker eşiği altı");
        assert!(!ofi_ok(false, 0.30, 0.60));
    }

    #[test]
    fn oi_katmani_karar_agaci() {
        assert_eq!(oi_verdict(-2_500.0, 1_000_000.0), OiVerdict::Ok(0.0025));
        assert_eq!(
            oi_verdict(-800.0, 1_000_000.0),
            OiVerdict::Noise,
            "eşik altı"
        );
        assert_eq!(
            oi_verdict(3_000.0, 1_000_000.0),
            OiVerdict::Rising,
            "stop avı değil"
        );
        assert_eq!(oi_verdict(0.0, 1_000_000.0), OiVerdict::Noise);
        assert_eq!(oi_verdict(f64::NAN, 1_000_000.0), OiVerdict::Noise);
    }

    #[test]
    fn rejim_yon_uyumu() {
        assert!(regime_permits(RegimeLabel::Trend { up: true }, true));
        assert!(!regime_permits(RegimeLabel::Trend { up: true }, false));
        assert!(regime_permits(RegimeLabel::Trend { up: false }, false));
        // SMC-L: yalnız TREND yönünde; MR/GEÇİŞ/BLOOKED rejimde sinyal yok
        assert!(!regime_permits(RegimeLabel::MeanReversion, true));
        assert!(!regime_permits(RegimeLabel::Gecis, true));
        assert!(!regime_permits(RegimeLabel::VolSpike, true));
        assert!(!regime_permits(RegimeLabel::Squeeze, true));
    }

    #[test]
    fn veri_yoksa_skip() {
        let mut m = onay_input(long_series());
        m.book_imbalance = None;
        m.taker_ratio = None;
        assert!(
            matches!(SmcL.evaluate(&m), Signal::Skip { .. }),
            "OFI verisi yok"
        );
        let mut m2 = onay_input(long_series());
        m2.oi_delta = None;
        assert!(matches!(SmcL.evaluate(&m2), Signal::Skip { .. }), "ΔOI yok");
        let mut m3 = onay_input(long_series());
        m3.ctx = None;
        assert!(
            matches!(SmcL.evaluate(&m3), Signal::Skip { .. }),
            "OI seviyesi yok"
        );
    }

    #[test]
    fn yetersiz_mum_penceresi_skip() {
        let cs = flat(MIN_BARS - 1, 100.0);
        let m = MarketInput {
            coin: "T".into(),
            candles: cs,
            ..Default::default()
        };
        assert!(matches!(SmcL.evaluate(&m), Signal::Skip { .. }));
    }

    #[test]
    fn deterministik() {
        let a = SmcL.evaluate(&onay_input(long_series()));
        let b = SmcL.evaluate(&onay_input(long_series()));
        assert_eq!(a, b);
    }
}
