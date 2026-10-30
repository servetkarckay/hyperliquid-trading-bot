//! REGIME DETECTION — piyasa rejimi sınıflandırma (motor kapısı + oy).
//!
//! KANONİK BİLEŞENLER (kaynaklar):
//! - **ADX** — J. Welles Wilder, "New Concepts in Technical Trading
//!   Systems" (1978): ADX > 25 = trend piyasası, ADX < 20 = trendsiz
//!   (kanonik Wilder eşikleri).
//! - **Hurst üssü** — H.E. Hurst (1951, R/S analizi); Harold Edwin/
//!   Benoit Mandelbrot yorumu: H > 0.5 = kalıcı (trend), H < 0.5 =
//!   anti-kalıcı (mean-reversion), H = 0.5 = rastgele yürüyüş.
//!   0.55/0.45 tamponları 0.5 sınırının kalibre tamponudur.
//! - **Vol yüzdelik rejimi**: 30-bar vol'un 200-bar geçmiş içindeki
//!   yüzdeliği — %90 üstü = vol-spike, %10 altı = squeeze (yüzdelik
//!   bazlı rejim sınıflandırma — standart pratik).
//!
//! YÖN KAPLAMASI (GARCH ile aynı dürüstlük kontratı): TREND rejiminde
//! 25-bar momentum yönü — rejim teorisinin kendisi YÖN VERMEZ; kaplama
//! notta AÇIKÇA işaretli.
//!
//! MOTOR KAPISI (`classify`): rejim → RegimeGate (Trend→TrendBoost;
//! Gecis/MR→Open; VolSpike/Squeeze→Blocked). Hesaplanamazsa None →
//! motor güvenli Open (sahte 0/0.5 ile kapı YÖNETİLMEZ).
//!
//! TEK KAYNAK: strateji oy'u ve motor kapısı AYNI karar ağacını
//! (`classify`) kullanır — iki kopya karar ağacı sapamaz.
//! <200 mum → Skip/None; deterministik, replay-idempotent, panik yok.
use data::Candle;
use math::indicators;
use math::stats;

/// Yapılandırılmış rejim etiketi — motor/ensemble entegrasyonu için.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegimeLabel {
    Trend { up: bool },
    MeanReversion,
    VolSpike,
    Squeeze,
    Gecis,
}

impl RegimeLabel {
    pub fn gate(self) -> crate::ensemble::RegimeGate {
        use crate::ensemble::RegimeGate as G;
        match self {
            RegimeLabel::Trend { .. } => G::TrendBoost,
            RegimeLabel::Gecis | RegimeLabel::MeanReversion => G::Open,
            RegimeLabel::VolSpike | RegimeLabel::Squeeze => G::Blocked,
        }
    }
}

/// Strateji ile aynı hesapları kullanan halka açık sınıflandırıcı.
/// TEK KAYNAK karar ağacı: (etiket, not) — strateji oy'u ve motor
/// kapısı aynı fonksiyonu kullanır (iki kopya ağaç sapamaz).
pub(crate) fn decide(adx: f64, hurst: f64, vol_pct: f64, up: bool) -> (RegimeLabel, String) {
    if adx > 25.0 && hurst > 0.55 {
        (
            RegimeLabel::Trend { up },
            format!("TREND: ADX={adx:.1} H={hurst:.2}"),
        )
    } else if adx < 20.0 && hurst < 0.45 {
        (
            RegimeLabel::MeanReversion,
            format!("MR: ADX={adx:.1} H={hurst:.2}"),
        )
    } else if vol_pct > 0.9 {
        (
            RegimeLabel::VolSpike,
            format!("VOL_SPIKE: %{:.0}", vol_pct * 100.0),
        )
    } else if vol_pct < 0.1 {
        (
            RegimeLabel::Squeeze,
            format!("SQUEEZE: %{:.0}", vol_pct * 100.0),
        )
    } else {
        (
            RegimeLabel::Gecis,
            format!(
                "GEÇİŞ: ADX={adx:.1} H={hurst:.2} vol%{:.0}",
                vol_pct * 100.0
            ),
        )
    }
}

/// vol yüzdelik: 30-bar vol'un 200-bar geçmiş içindeki yüzdeliği.
pub(crate) fn vol_percentile(r: &[f64]) -> f64 {
    let vol30 = |end: usize| -> f64 {
        let w = &r[end - 30..end];
        (w.iter().map(|x| x * x).sum::<f64>() / 30.0).sqrt()
    };
    let cur = vol30(r.len());
    let hist: Vec<f64> = (30..r.len()).step_by(10).map(vol30).collect();
    hist.iter().filter(|v| **v <= cur).count() as f64 / hist.len() as f64
}

/// TrendBoost bilinçli sınır: kütüphane pozisyonu "TREND" = TrendBoost.
/// (strateji oy'su ayrı; bu eşleme RegimeLabel::gate'te tanımlı)
pub fn classify(candles: &[Candle]) -> Option<(RegimeLabel, String)> {
    use crate::closes;
    let c = closes(candles);
    if c.len() < 200 {
        return None;
    }
    let h: Vec<f64> = candles.iter().map(|x| x.h).collect();
    let l: Vec<f64> = candles.iter().map(|x| x.l).collect();

    // VERİ DÜRÜSTLÜĞÜ: bozuk göstergenin sesiyle kapı YÖNETİLMEZ —
    // ADX/Hurst hesaplanamazsa None → motor güvenli varsayıma (Open) döner;
    // geçerli hesapla TREND kapısı yalnız gerçek veriyle açılır.
    let adx = indicators::adx(&h, &l, &c, 14)
        .2
        .last()
        .and_then(|v| *v)
        .filter(|v| v.is_finite() && *v > 0.0)?;
    let h_ = stats::hurst(&c, 20).filter(|v| v.is_finite())?;
    let r = indicators::log_returns(&c);
    let pct = vol_percentile(&r);
    let up = c[c.len() - 1] > c[c.len() - 25];

    let (label, detail) = decide(adx, h_, pct, up);
    Some((label, detail))
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    /// `classify` — motor kapısı: rejim etiketi → RegimeGate eşleşmesi.
    #[test]
    fn rejim_gate_eslesmesi() {
        use crate::ensemble::RegimeGate as G;
        assert_eq!(RegimeLabel::Trend { up: true }.gate(), G::TrendBoost);
        assert_eq!(RegimeLabel::MeanReversion.gate(), G::Open);
        assert_eq!(RegimeLabel::Gecis.gate(), G::Open);
        assert_eq!(RegimeLabel::VolSpike.gate(), G::Blocked);
        assert_eq!(RegimeLabel::Squeeze.gate(), G::Blocked);
    }
}
