//! SMC-L MODÜLÜ — Sweep+CISD + OFI + OI + Rejim tek sinyalde.
//!
//! Katman düzeni (veri varlığı → çekirdek → rejim → OFI → OI):
//! 1. **Çekirdek ICT kurulumu** (`sweep_cisd::plan`): likidite sweep fitili
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
//!
//! Veri yoksa `Skip` (sahte sinyal yok), katman ihlali `Hold` (bilinçli
//! bekleme). Deterministik, replay-idempotent, panik yok.
use crate::regime_detection::{classify, RegimeLabel};
use crate::sweep_cisd::{Plan, SweepCisd, MIN_BARS};
use crate::{MarketInput, Signal, Strategy};
use data::Candle;

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
        // ── 4) OI: sweep = stop patlaması → ΔOI < 0, görece ≥ %0.2
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
