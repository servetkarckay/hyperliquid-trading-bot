//! MRE — MEAN-REVERSION & OVERREACTION ENGINE (Aşırı Sapma Modülü).
//!
//! 3 aşırı-tepki stratejisinin KATMAN ZİNCİRİ (SMC-L/MMT ile aynı
//! dürüst mimari; bir katman vetoes → sinyal yok):
//!
//! 1. **Rejim**: `classify` TREND rejimini RED (trendless/çalkantılı
//!    pazarda geçerli); VolSpike/Squeeze (Blocked) da geçmez. MR/GEÇİŞ
//!    (Open) onaylı.
//! 2. **De Bondt-Thaler Z**: 6-bar aşırı hareket, 100-bar log-return
//!    RMS normalize — Z ≤ −2.5 panik (kontra LONG) / Z ≥ +2.5 euphoria
//!    (kontra SHORT). (Doküman formülü: Z-Score(Price) < −2.5.)
//! 3. **GARCH σ teyidi**: GARCH(1,1) koşullu vol — σ_next/σ_long-run
//!    ≥ 1.0: anomali volatil patlama yaratsın (doğal oran 1.0 eşiği;
//!    ayar yok). De Bondt-Thaler'ın hacim-teyid katmanının volatilite
//!    karşılığı burada devralındı.
//! 4. **Cross-Sectional rank**: kesit quintile — panik uzvu → evrenin
//!    alt %40'ında (Bottom/BottomShort), euphoria → üst %20 (Top).
//!    Orta bölgeler (Mid/adı yok) → Hold.
//!
//! Fiyat planı: hedef = aşırı hareketin başlangıcı (c[last−6]) —
//! "ortalamaya dönüş" literal hedefi; SL = 1.5×ATR14 risk tavanı.
//!
//! Deterministik, replay-idempotent, panik yok; yetersiz veri → `Skip`.
use crate::closes;
use crate::regime_detection::{classify, RegimeLabel};
use crate::{MarketInput, Signal, Strategy};
use data::Candle;
use math::indicators;

pub struct Mre;

/// Tarama ayar paketi — kanonik eşikler = Default; optimizer bin'leri
/// geçici varyant olarak kullanır.
#[derive(Debug, Clone, Copy)]
pub struct MreTune {
    pub z_esik: f64,
    pub sigma_ratio: f64,
    pub sl_mult: f64,
}
impl Default for MreTune {
    fn default() -> Self {
        Self {
            z_esik: Z_ESIK,
            sigma_ratio: SIGMA_RATIO_ESIK,
            sl_mult: 1.5,
        }
    }
}

pub const MIN_BARS: usize = 200;
const Z_ESIK: f64 = 2.5;
const SIGMA_RATIO_ESIK: f64 = 1.0;
const ATR_N: usize = 14;
const MOVE_N: usize = 6;
const SD_WINDOW: usize = 100;

/// Coinin kesit sıralamasındaki **dilimi** — tek kaynak (strateji +
/// engine + replay aynı fonksiyonu kullanır).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankBand {
    /// üst %20
    Top,
    /// orta (%20-%80 arası karışım; adı geçen bantlar Hariç demektir)
    Mid,
    /// %60-%80 (alt-ikinci quintile)
    Bottom,
    /// alt %20 (en düşük quintile)
    BottomShort,
}

/// Coinin sıralama dilimi (`min_n` = evren güvenilirlik eşiği; kanonik
/// kontrat ≥20 coin). `None` = coin listede yok / evren küçük.
///
/// Sıralama azalan getiri; üst %20 → `Top`, alt %20 → `BottomShort`.
pub fn rank_band_at(universe_returns: &[(String, f64)], coin: &str) -> Option<RankBand> {
    let mut ranked = universe_returns.to_vec();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let rank = ranked.iter().position(|(c, _)| c == coin)?;
    let n = ranked.len();
    let top_k = (n as f64 * 0.2).ceil() as usize;
    let pct = rank as f64 / n as f64;
    if rank < top_k {
        Some(RankBand::Top)
    } else if rank >= n - top_k {
        Some(RankBand::BottomShort)
    } else if pct > 0.4 && pct < 0.6 {
        Some(RankBand::Mid)
    } else if pct > 0.6 {
        Some(RankBand::Bottom)
    } else {
        Some(RankBand::Mid)
    }
}

/// Kanonik kontrat: <20 coine sıralama güvenilir değil → None.
pub fn rank_band(universe_returns: &[(String, f64)], coin: &str) -> Option<RankBand> {
    if universe_returns.len() < 20 {
        return None;
    }
    rank_band_at(universe_returns, coin)
}

/// Fiyat planı (replay/doğrulama için açık).
#[derive(Debug, Clone, PartialEq)]
pub struct ReversionPlan {
    pub long: bool,
    pub entry: f64,
    pub sl: f64,
    pub tp: f64,
    pub strength: f64,
    pub note: String,
}

/// Replay kontratı: üretim zinciriyle birebir AYNI (Z → rejim → GARCH
/// → kesit) ama kesit bantları `rank_band_at` ile — tarihsel veri kümesi
/// 8 coin (kanonik ≥20 kontratı replay küçük evrenünde bilinçli
/// esner; dokümante sapma, eşik AYARLAMASI değildir).
pub fn plan_replay(
    candles: &[Candle],
    universe_returns: &[(String, f64)],
    coin: &str,
) -> Option<ReversionPlan> {
    replay_chain(candles, universe_returns, coin, &MreTune::default())
}

pub fn replay_chain(
    candles: &[Candle],
    universe_returns: &[(String, f64)],
    coin: &str,
    tune: &MreTune,
) -> Option<ReversionPlan> {
    let (label, _) = regime_label(candles)?;
    if !regime_permits_mre(&label) {
        return None;
    }
    let (z, long) = z_overreaction(candles, tune.z_esik)?;
    let ratio = garch_threshold(candles, tune.sigma_ratio)?;
    if !(ratio.is_finite() && ratio >= tune.sigma_ratio) {
        return None;
    }
    let band = rank_band_at(universe_returns, coin)?;
    let band_ok = if long {
        matches!(band, RankBand::Bottom | RankBand::BottomShort)
    } else {
        band == RankBand::Top
    };
    if !band_ok {
        return None;
    }
    build_plan(candles, z, long, ratio, band, tune.sl_mult)
}

fn build_plan(
    candles: &[Candle],
    z: f64,
    long: bool,
    ratio: f64,
    band: RankBand,
    sl_mult: f64,
) -> Option<ReversionPlan> {
    let c = closes(candles);
    let last = c.len() - 1;
    let entry = c[last];
    let tp = c[last - MOVE_N];
    let h: Vec<f64> = candles.iter().map(|x| x.h).collect();
    let l: Vec<f64> = candles.iter().map(|x| x.l).collect();
    let atr = indicators::atr(&h, &l, &c, ATR_N)
        .last()
        .and_then(|v| *v)
        .filter(|v| v.is_finite() && *v > 0.0)?;
    let sl = if long {
        entry - sl_mult * atr
    } else {
        entry + sl_mult * atr
    };
    let strength = (0.5 + (z.abs() - Z_ESIK) / 5.0).clamp(0.35, 0.9);
    let note =
        format!("MRE katmanları ✓ · Z={z:+.2}σ · σ_oranı={ratio:.2} · rank={band:?} · rejim TAMAM");
    Some(ReversionPlan {
        long,
        entry,
        sl,
        tp,
        strength,
        note,
    })
}

impl Strategy for Mre {
    fn name(&self) -> &'static str {
        "MRE"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        if input.candles.len() < MIN_BARS {
            return Signal::Skip {
                reason: "MRE için >=200 mum gerekli".into(),
            };
        }
        if input.universe_returns.len() < 20 {
            return Signal::Skip {
                reason: "MRE için evren getirisi < 20 coin".into(),
            };
        }
        let Some(plan) = Self::plan_entity(&input.candles, &input.universe_returns, &input.coin)
        else {
            return Signal::Hold {
                note: "MRE: katman zinciri tamamlanmadı (rejim/Z/σ/rank onayı eksik)".into(),
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

impl Mre {
    /// evaluate gövdesi — test/replay için de açık (kanonik; MreTune::default).
    pub fn plan_entity(
        candles: &[Candle],
        universe_returns: &[(String, f64)],
        coin: &str,
    ) -> Option<ReversionPlan> {
        Self::plan_tuned(candles, universe_returns, coin, &MreTune::default())
    }

    /// Parametrized zincir (optimizer bin'leri için).
    pub fn plan_tuned(
        candles: &[Candle],
        universe_returns: &[(String, f64)],
        coin: &str,
        tune: &MreTune,
    ) -> Option<ReversionPlan> {
        if candles.len() < MIN_BARS || universe_returns.len() < 20 {
            return None;
        }
        // ── 2) Z katmanı: yön burada belirlenir
        let (z, long) = z_overreaction(candles, tune.z_esik)?;
        // ── 1) Rejim: TREND/Blocked red (AND zincirinde geç katman ama
        //    erken eleme ucuz)
        let (label, _) = regime_label(candles)?;
        if !regime_permits_mre(&label) {
            return None;
        }
        // ── 3) GARCH koşullu vol teyidi
        let ratio = garch_threshold(candles, tune.sigma_ratio)?;
        if !(ratio.is_finite() && ratio >= tune.sigma_ratio) {
            return None;
        }
        // ── 4) Cross-sectional rank teyidi
        let band = rank_band(universe_returns, coin)?;
        let band_ok = if long {
            matches!(band, RankBand::Bottom | RankBand::BottomShort)
        } else {
            band == RankBand::Top
        };
        if !band_ok {
            return None;
        }
        // ── Fiyat planı: hedef = aşırı hareketin başlangıcı
        build_plan(candles, z, long, ratio, band, tune.sl_mult)
    }
}

/// Rejim etiketi (MRE: TREND ve Blocked red dediliyor).
fn regime_label(candles: &[Candle]) -> Option<(RegimeLabel, String)> {
    let input = MarketInput {
        coin: String::new(),
        candles: candles.to_vec(),
        ..MarketInput::default()
    };
    classify(&input)
}

fn regime_permits_mre(label: &RegimeLabel) -> bool {
    matches!(label, RegimeLabel::MeanReversion | RegimeLabel::Gecis)
}

/// De Bondt-Thaler Z katmanı: (z, long) — z ≤ −2.5 → panik long;
/// z ≥ +2.5 → euphoria short. Arasında → None.
pub fn z_overreaction(candles: &[Candle], z_esik: f64) -> Option<(f64, bool)> {
    let c = closes(candles);
    if c.len() < SD_WINDOW + 8 {
        return None;
    }
    let r = indicators::log_returns(&c);
    let sd = {
        let w = &r[r.len().saturating_sub(SD_WINDOW)..];
        (w.iter().map(|x| x * x).sum::<f64>() / w.len() as f64).sqrt()
    };
    let move6 = c[c.len() - 1] / c[c.len() - 1 - MOVE_N] - 1.0;
    let z = move6 / (sd * (MOVE_N as f64).sqrt()).max(1e-9);
    if z <= -z_esik {
        Some((z, true))
    } else if z >= z_esik {
        Some((z, false))
    } else {
        None
    }
}

/// GARCH(1,1) koşullu vol teyidi — σ_next/σ_long-run oranı.
/// Anomalinin volatilite patlaması yaratması şartı (≥1.0 doğal eşik).
pub fn garch_threshold(candles: &[Candle], _th: f64) -> Option<f64> {
    let c = closes(candles);
    if c.len() < MIN_BARS {
        return None;
    }
    let r = indicators::log_returns(&c);
    let from = r.len().saturating_sub(300);
    let (omega, alpha, beta, _ll) = indicators::garch11_fit(&r[from..], 20);
    let vars = indicators::garch11_variance(&r, omega, alpha, beta);
    let v = vars.last().copied().filter(|x| x.is_finite() && *x > 0.0)?;
    let sigma_next = v.sqrt();
    let sigma_longrun = (omega / (1.0 - alpha - beta).max(1e-9)).sqrt();
    if sigma_longrun.is_finite() && sigma_longrun > 0.0 {
        Some(sigma_next / sigma_longrun)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strat_test_helpers::helpers::{candle, flat};

    const H: i64 = 3_600_000;

    #[test]
    fn z_katmani_esik_ve_yon() {
        // yumuşak 100 bar + son 6 barda sert sıyırma → panik z (long)
        let mut cs = flat(110, 100.0);
        let depths = [-1.0, -2.0, -4.0, -6.0, -8.0, -10.0];
        for (k, d) in depths.iter().enumerate() {
            let i = 104 + k;
            cs[i] = candle((i as i64) * H, 100.0, 100.2, 100.0 + d, 100.0 + d, 1200.0);
        }
        let (z, long) = z_overreaction(&cs, Z_ESIK).expect("uygundur");
        assert!(long && z <= -2.5, "panik z={z}");
        // ayna: euphoria (short)
        let ups = [1.0, 2.0, 4.0, 6.0, 8.0, 10.0];
        let mut cs = flat(110, 100.0);
        for (k, d) in ups.iter().enumerate() {
            let i = 104 + k;
            cs[i] = candle(
                (i as i64) * H,
                100.0,
                100.0 + d,
                100.0 - 0.2,
                100.0 + d,
                1200.0,
            );
        }
        let (z, long) = z_overreaction(&cs, Z_ESIK).expect("uygundur");
        assert!(!long && z >= 2.5, "euphoria z={z}");
    }

    #[test]
    fn z_esik_alti_red() {
        // sakin seride z ~0 → None
        assert!(z_overreaction(&flat(120, 100.0), Z_ESIK).is_none());
    }

    #[test]
    fn garch_oran_hesaplanir() {
        // çalkantılı gerçekçi seri: oran hesaplanabilmeli
        let mut cs = Vec::new();
        for i in 0..220 {
            let amp = if i % 20 < 10 { 1.5 } else { 2.5 };
            let px = 100.0 + (if i % 2 == 0 { amp } else { -amp });
            cs.push(candle((i as i64) * H, px, px + 0.1, px - 0.1, px, 100.0));
        }
        let r = garch_threshold(&cs, SIGMA_RATIO_ESIK);
        assert!(r.is_some(), "oran hesaplanmalı");
        let _ = r.unwrap();
    }

    #[test]
    fn cs_rank_bantlari() {
        let mut u: Vec<(String, f64)> = (0..20).map(|i| (format!("C{i}"), -(i as f64))).collect();
        // C0 en yüksek getiri → Top; C19 en düşük → BottomShort
        assert_eq!(rank_band(&u, "C0"), Some(RankBand::Top));
        assert_eq!(rank_band(&u, "C19"), Some(RankBand::BottomShort));
        // evren küçükse kanonik kontrat None
        u.truncate(8);
        assert_eq!(rank_band(&u, "C0"), None, "<20 coin → None");
        // _at doğrudan çalışır (replay küçük evren için)
        assert_eq!(rank_band_at(&u, "C0"), Some(RankBand::Top));
    }

    #[test]
    fn rejim_kubbe() {
        // Trend etiketi red; MR/GEÇİŞ ileti.
        assert!(!regime_permits_mre(&RegimeLabel::Trend { up: true }));
        assert!(!regime_permits_mre(&RegimeLabel::VolSpike));
        assert!(regime_permits_mre(&RegimeLabel::MeanReversion));
        assert!(regime_permits_mre(&RegimeLabel::Gecis));
    }
}
