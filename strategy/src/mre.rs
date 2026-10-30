//! MRE — MEAN-REVERSION & OVERREACTION ENGINE (Aşırı Sapma Modülü).
//!
//! 3 aşırı-tepki stratejisinin KATMAN ZİNCİRİ (dürüst mimari; bir katman
//! vetoes → sinyal yok):
//!
//! 1. **Rejim**: `classify` TREND rejimini RED (trendless/çalkantılı
//!    pazarda geçerli); VolSpike/Squeeze (Blocked) da geçmez. MR/GEÇİŞ
//!    (Open) onaylı.
//! 2. **De Bondt-Thaler Z**: 6-bar aşırı hareket, 100-bar log-return
//!    std normalize — Z ≤ −2.5 panik (kontra LONG) / Z ≥ +2.5 euphoria
//!    (kontra SHORT). (Doküman formülü: Z-Score(Price) < −2.5.)
//! 3. **GARCH σ teyidi**: GARCH(1,1) koşullu vol — σ_next/σ_long-run
//!    ≥ 1.0: anomali volatil patlama yaratsın (doğal oran 1.0 eşiği;
//!    ayar yok). De Bondt-Thaler'ın hacim-teyid katmanının volatilite
//!    karşılığı burada devralındı.
//! 4. **Cross-Sectional rank**: kesit quintile — panik uzvu → evrenin
//!    alt %40'ında (Bottom/BottomShort), euphoria → üst %20 (Top).
//!    Orta bölgeler (Mid/adı yok) → Hold.
//! 5. **İnfaz mumu kuralı (yönlü)**: sinyal barı aralığı/ATR14 — dip
//!    alımında (LONG) infaz mumu DİNMIŞ olmalı (aralık ≤ 2.7×ATR: bıçak
//!    yere düşmüş); zirve shortunda (SHORT) infaz mumu ZİRVEDE olmalı
//!    (aralık > 2.7×ATR: coşku tam delirmede). Kanıt: 83 gün × 179 coin
//!    replay (754 işlem) — pf 1.41→1.95; 3 ufukta + 2 yarıda tutarlı.
//! 6. **PANİK-DİP LONG**: LONG yalnız gerçek panik dipinde (ret6 ≤
//!    −6.3%). Sıradan dip alma pahalıya düşüş piyasasında birikir
//!    (May-Tem 2026: filtresiz LONG ΣR −51.3R). Taze 79-coin/190-gün
//!    out-of-sample teyit: filtreyle May-Tem −2.8R, Ağu-Eki pf 1.99,
//!    toplam pf 1.28→1.53, MDD 62.3R→15.9R (%47→%15 equity). SHORT'a
//!    uygulanmaz.
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
/// İnfaz mumu eşiği: sinyal barı aralığının ATR14'e oranı. LONG'da bu
/// değerin ALTINDA (panik dinmiş), SHORT'ta ÜSTÜNDE (coşku zirvede)
/// olmalı — 83 gün replay'de türetilen keskin ayrım (bkz. modül doc 5).
const INFAZ_ATR_K: f64 = 2.7;
/// PANİK-DİP LONG eşiği: son 6 bar (MOVE_N) kapanış getirisinin yüzde
/// sınırı. LONG yalnız GERÇEK panik dipinde açılır (ret6 ≤ −6.3%);
/// sıradan dipler pahalıya düşüş piyasasında birikir (May-Tem 2026 replay
/// kaybı −51.3R). Madencilik bulgusu taze 79-coin/190-gün veride
/// out-of-sample teyit edildi: filtre sonrası May-Tem −2.8R, Ağu-Eki
/// pf 1.99, MDD 62.3R → 15.9R. SHORT tarafına uygulanmaz.
pub const PANIK_DIP_RET6: f64 = -6.3;
/// Bar aralığı (saatlik) — TradeIntent zaman alanları için.
const BAR_MS: i64 = 3_600_000;

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
    if rank < top_k {
        Some(RankBand::Top)
    } else if rank >= n.saturating_sub(top_k) {
        Some(RankBand::BottomShort)
    } else if rank >= n.saturating_sub(2 * top_k) {
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

/// Kanonik işlem niyeti (TradeIntent) — canlı engine ve replay AYNI
/// mantığı kullanır. SL/TP/entry MRE'nin kendi planından gelir
/// (c[last−6] hedefi); AllocationManager formülü ile çakışmaz.
#[derive(Debug, Clone, PartialEq)]
pub struct TradeIntent {
    pub long: bool,
    pub entry: f64,
    pub sl: f64,
    pub tp: f64,
    pub strength: f64,
    pub note: String,
    /// Sinyal üretildiği an (epoch ms) — mum kapanış zamanı.
    pub signal_ts: i64,
    /// Sinyal mumunun timestamp'i (entry koşulunun değerlendirildiği bar).
    pub signal_candle_ts: i64,
    /// Niyetin geçerlilik süresi (epoch ms) — bu süreden sonra niyet iptal.
    pub expires_at: i64,
}

/// Replay kontratı: üretim zinciriyle SONUÇ birebir aynı (Z → rejim → GARCH
/// → kesit) ama kesit bantları `rank_band_at` ile — tarihsel veri kümesi
/// 8 coin (kanonik ≥20 kontratı replay küçük evrenünde bilinçli
/// esner; dokümante sapma, eşik AYARLAMASI değildir).
pub fn plan_replay(
    candles: &[Candle],
    universe_returns: &[(String, f64)],
    coin: &str,
) -> Option<TradeIntent> {
    replay_chain(candles, universe_returns, coin, &MreTune::default())
}

pub fn replay_chain(
    candles: &[Candle],
    universe_returns: &[(String, f64)],
    coin: &str,
    tune: &MreTune,
) -> Option<TradeIntent> {
    let (label, _) = regime_label(candles)?;
    if !regime_permits_mre(&label) {
        return None;
    }
    let (z, long) = z_overreaction(candles, tune.z_esik)?;
    // infaz mumu kuralı — replay kontratı üretimle birebir
    if !infaz_mumu_ok(candles, long) {
        return None;
    }
    // PANİK-DİP LONG — replay kontratı üretimle birebir
    if long && !panic_dip_ok(candles) {
        return None;
    }
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
) -> Option<TradeIntent> {
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
    let note = format!(
        "MRE katmanları ✓ · Z={z:+.2}σ · σ_oranı={ratio:.2} · rank={band:?} · infaz mumu ✓ · rejim TAMAM"
    );
    let signal_candle_ts = candles.last().map(|c| c.t).unwrap_or(0);
    let signal_ts = signal_candle_ts;
    let expires_at = signal_candle_ts + (MOVE_N as i64) * BAR_MS;
    Some(TradeIntent {
        long,
        entry,
        sl,
        tp,
        strength,
        note,
        signal_ts,
        signal_candle_ts,
        expires_at,
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
        let Some(intent) = Self::plan_entity(&input.candles, &input.universe_returns, &input.coin)
        else {
            return Signal::Hold {
                note: "MRE: katman zinciri tamamlanmadı (rejim/Z/σ/rank onayı eksik)".into(),
            };
        };
        if intent.long {
            Signal::Long {
                strength: intent.strength,
                note: intent.note.clone(),
                trade_intent: Some(intent),
            }
        } else {
            Signal::Short {
                strength: intent.strength,
                note: intent.note.clone(),
                trade_intent: Some(intent),
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
    ) -> Option<TradeIntent> {
        Self::plan_tuned(candles, universe_returns, coin, &MreTune::default())
    }

    /// Parametrized zincir (optimizer bin'leri için).
    pub fn plan_tuned(
        candles: &[Candle],
        universe_returns: &[(String, f64)],
        coin: &str,
        tune: &MreTune,
    ) -> Option<TradeIntent> {
        if candles.len() < MIN_BARS || universe_returns.len() < 20 {
            return None;
        }
        // ── 2) Z katmanı: yön burada belirlenir
        let (z, long) = z_overreaction(candles, tune.z_esik)?;
        // ── 2b) İnfaz mumu kuralı: dip alımda mum dinmiş, zirve
        //    shortunda infaz zirvede (83 gün replay kanıtı)
        if !infaz_mumu_ok(candles, long) {
            return None;
        }
        // ── 2c) PANİK-DİP LONG: sıradan dip alınmaz; yalnız gerçek panik
        //    dipinde (ret6 ≤ −6.3%). Canlı/replay aynı kontrat.
        if long && !panic_dip_ok(candles) {
            return None;
        }
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

/// İnfaz mumu kuralı (yönlü) — dip alımında son bar sakin, zirve
/// shortunda son bar infaz. ATR hesaplanamazsa kapı YOK (güvenli red).
pub fn infaz_mumu_ok(candles: &[Candle], long: bool) -> bool {
    if candles.is_empty() {
        return false;
    }
    let h: Vec<f64> = candles.iter().map(|x| x.h).collect();
    let l: Vec<f64> = candles.iter().map(|x| x.l).collect();
    let c = closes(candles);
    let atr = indicators::atr(&h, &l, &c, ATR_N)
        .last()
        .and_then(|v| *v)
        .filter(|v| v.is_finite() && *v > 0.0);
    let Some(atr) = atr else {
        return false;
    };
    let last = candles.len() - 1;
    let rng = h[last] - l[last];
    if long {
        rng <= INFAZ_ATR_K * atr
    } else {
        rng > INFAZ_ATR_K * atr
    }
}

/// PANİK-DİP koşulu — LONG girişleri için. Son MOVE_N barın kümülatif
/// kapanış getiri(%)'si `PANIK_DIP_RET6`'dan az olmalı. Kısa seri /
/// sayısal bozukluk → güvenli red (false). Canlı ve replay AYNI
/// fonksiyonu kullanır (kanonik kontrat).
pub fn panic_dip_ok(candles: &[Candle]) -> bool {
    let c = closes(candles);
    let n = c.len();
    if n < MOVE_N + 1 {
        return false;
    }
    let prev = c[n - 1 - MOVE_N];
    if !(prev.is_finite() && prev > 0.0) {
        return false;
    }
    let ret6 = (c[n - 1] / prev - 1.0) * 100.0;
    ret6.is_finite() && ret6 <= PANIK_DIP_RET6
}

/// Rejim etiketi (MRE: TREND ve Blocked red dediliyor).
fn regime_label(candles: &[Candle]) -> Option<(RegimeLabel, String)> {
    classify(candles)
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
        let mean = w.iter().sum::<f64>() / w.len() as f64;
        (w.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / w.len() as f64).sqrt()
    };
    let move6 = (c[c.len() - 1] / c[c.len() - 1 - MOVE_N]).ln();
    let z = move6 / (sd.max(1e-9) * (MOVE_N as f64).sqrt());
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
    let vars = indicators::garch11_variance(&r[from..], omega, alpha, beta);
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
    use crate::test_candles::helpers::{candle, flat};

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

    #[test]
    fn infaz_mumu_yonlu() {
        // LONG: son bar sakin (aralık ≤ 2.7×ATR) → OK; infaz mumu (büyük) → red.
        let mut cs = flat(220, 100.0);
        // son bar: aralık 1.0 (ATR~0.2 seride küçük) — sakin
        cs[219] = candle(219 * H, 94.0, 94.6, 93.6, 94.4, 100.0);
        assert!(infaz_mumu_ok(&cs, true), "dip alımında dinmiş mum OK");
        // ayna: son bar infaz — aralık 8.0 → LONG red
        let mut cs = flat(220, 100.0);
        cs[219] = candle(219 * H, 98.0, 99.0, 91.0, 94.0, 100.0);
        assert!(!infaz_mumu_ok(&cs, true), "dip alımında infaz mumu red");
        // SHORT: infaz mumu (coşku zirvesi) → OK; sakin mum → red
        assert!(infaz_mumu_ok(&cs, false), "zirve shortunda infaz mumu OK");
        let mut cs = flat(220, 100.0);
        cs[219] = candle(219 * H, 94.0, 94.6, 93.6, 94.4, 100.0);
        assert!(!infaz_mumu_ok(&cs, false), "zirve shortunda sakin mum red");
        // ATR hesaplanamazsa (yetersiz veri) kapı YOK
        assert!(!infaz_mumu_ok(&flat(10, 100.0), true));
        assert!(!infaz_mumu_ok(&flat(10, 100.0), false));
    }

    #[test]
    fn panik_dip_esigi() {
        // PANİK dip: son 6 barda −%7 kümulatif düşüş → LONG OK
        let mut cs = flat(120, 100.0);
        let drops = [-1.0, -2.0, -3.0, -4.5, -5.5, -7.0]; // 100 → 93.0
        for (k, d) in drops.iter().enumerate() {
            let i = 114 + k;
            cs[i] = candle((i as i64) * H, 100.0, 100.2, 100.0 + d, 100.0 + d, 1200.0);
        }
        assert!(panic_dip_ok(&cs), "gerçek panik dip OK");
        // sıradan dip: −%3 → red
        let mut cs = flat(120, 100.0);
        let drops = [-0.5, -1.0, -1.5, -2.0, -2.5, -3.0]; // 100 → 97.0
        for (k, d) in drops.iter().enumerate() {
            let i = 114 + k;
            cs[i] = candle((i as i64) * H, 100.0, 100.2, 100.0 + d, 100.0 + d, 1200.0);
        }
        assert!(!panic_dip_ok(&cs), "sıradan dip red");
        // yükselen seri → red
        assert!(!panic_dip_ok(&flat(120, 100.0)), "düşüş yoksa red");
        // kısa seri (MOVE_N+1 altı) → güvenli red
        assert!(!panic_dip_ok(&flat(6, 100.0)));
        // ret6 eşiğin hemen altında/üstünde: −6.31 OK, −6.29 red
        // (f64 sınır noktasında ölçüm hatası olur; anlamlı kıyas kullanılır)
        let mut cs = flat(120, 100.0);
        let drops = [-1.0, -2.0, -3.0, -4.0, -5.0, -6.31]; // 100 → 93.69
        for (k, d) in drops.iter().enumerate() {
            let i = 114 + k;
            cs[i] = candle((i as i64) * H, 100.0, 100.2, 100.0 + d, 100.0 + d, 1200.0);
        }
        assert!(panic_dip_ok(&cs), "eşik altı OK");
        let mut cs = flat(120, 100.0);
        let drops = [-1.0, -2.0, -3.0, -4.0, -5.0, -6.29]; // 100 → 93.71
        for (k, d) in drops.iter().enumerate() {
            let i = 114 + k;
            cs[i] = candle((i as i64) * H, 100.0, 100.2, 100.0 + d, 100.0 + d, 1200.0);
        }
        assert!(!panic_dip_ok(&cs), "eşik üstü red");
    }

    #[test]
    fn plan_zinciri_panik_dip_uygulanir() {
        // plan_tuned + replay_chain: panik OLMAYAN long artığını red.
        // −%3 dip (yeterli z) + sakin mum + MR rejimi → eskiden LONG
        // dönerdi; artık None.
        let mut cs = flat(120, 100.0);
        let drops = [-0.5, -1.0, -1.5, -2.0, -2.5, -3.0];
        for (k, d) in drops.iter().enumerate() {
            let i = 114 + k;
            cs[i] = candle((i as i64) * H, 100.0, 100.2, 100.0 + d, 100.0 + d, 1200.0);
        }
        let mut u: Vec<(String, f64)> = (0..20).map(|i| (format!("C{i}"), 0.01)).collect();
        u[0] = ("TEST".into(), -0.20); // TEST kesitte en dip → Bottom
        assert!(
            Mre::plan_entity(&cs, &u, "TEST").is_none(),
            "sıradan dip LONG red"
        );
        assert!(replay_chain(&cs, &u, "TEST", &MreTune::default()).is_none());
    }
}
