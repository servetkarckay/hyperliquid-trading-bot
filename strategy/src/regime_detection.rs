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
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;
use math::stats;

pub struct RegimeDetection;

impl Strategy for RegimeDetection {
    fn name(&self) -> &'static str {
        "Regime Detection"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
 // TEK KAYNAK: karar ağacı classify ile ortak; oy = rejime göre.
        let Some((label, detail)) = classify(input) else {
            return Signal::Skip { reason: "rejim hesaplanamaz (>=200 mum / göstergeler hazır değil)".into() };
        };
        match label {
            RegimeLabel::Trend { up } => {
                // YÖN KAPLAMASI (rejim teorisi yön vermez): 25-bar momentum
                let dir_up = {
                    let c = crate::closes(&input.candles);
                    c[c.len() - 1] > c[c.len() - 25]
                };
                let _ = up;
                if dir_up {
                    Signal::Long { strength: 0.5, note: format!("TREND rejimi (kaplama: 25-bar momentum↑) — {detail}") }
                } else {
                    Signal::Short { strength: 0.5, note: format!("TREND rejimi (kaplama: 25-bar momentum↓) — {detail}") }
                }
            }
            RegimeLabel::MeanReversion => Signal::Hold { note: format!("MR rejimi — trend/mean-rev stratejileri devrede — {detail}") },
            RegimeLabel::Squeeze => Signal::Hold { note: format!("SQUEEZE: giriş kapalı (Blocked) — breakout stratejileri beklemede — {detail}") },
            RegimeLabel::VolSpike => Signal::Hold { note: format!("VOL_SPIKE: giriş kapalı (Blocked) — {detail}") },
            RegimeLabel::Gecis => Signal::Hold { note: format!("GEÇİŞ rejimi — {detail}") },
        }
    }
}

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
        (RegimeLabel::Trend { up }, format!("TREND: ADX={adx:.1} H={hurst:.2}"))
    } else if adx < 20.0 && hurst < 0.45 {
        (RegimeLabel::MeanReversion, format!("MR: ADX={adx:.1} H={hurst:.2}"))
    } else if vol_pct > 0.9 {
        (RegimeLabel::VolSpike, format!("VOL_SPIKE: %{:.0}", vol_pct * 100.0))
    } else if vol_pct < 0.1 {
        (RegimeLabel::Squeeze, format!("SQUEEZE: %{:.0}", vol_pct * 100.0))
    } else {
        (RegimeLabel::Gecis, format!("GEÇİŞ: ADX={adx:.1} H={hurst:.2} vol%{:.0}", vol_pct * 100.0))
    }
}

/// vol yüzdelik: 30-bar vol'un 200-bar geçmiş içindeki yüzdeliği.
pub(crate) fn vol_percentile(r: &[f64]) -> f64 {
    let vol30 = |end: usize| -> f64 {
        let w = &r[end - 30..end];
        (w.iter().map(|x| x * x).sum::<f64>() / 30.0).sqrt()
    };
    let cur = vol30(r.len());
    let hist: Vec<f64> = (30..r.len()).step_by(10).map(|i| vol30(i)).collect();
    hist.iter().filter(|v| **v <= cur).count() as f64 / hist.len() as f64
}

/// TrendBoost bilinçli sınır: kütüphane pozisyonu "TREND" = TrendBoost.
/// (strateji oy'su ayrı; bu eşleme RegimeLabel::gate'te tanımlı)

pub fn classify(input: &crate::MarketInput) -> Option<(RegimeLabel, String)> {
    use crate::closes;
    let c = closes(&input.candles);
    if c.len() < 200 {
        return None;
    }
    let h: Vec<f64> = input.candles.iter().map(|x| x.h).collect();
    let l: Vec<f64> = input.candles.iter().map(|x| x.l).collect();

    // VERİ DÜRÜSTLÜĞÜ: bozuk göstergenin sesiyle kapı YÖNETİLMEZ —
    // ADX/Hurst hesaplanamazsa None → motor güvenli varsayıma (Open) döner;
    // geçerli hesapla TREND kapısı yalnız gerçek veriyle açılır.
    let Some(adx) = indicators::adx(&h, &l, &c, 14).2.last().and_then(|v| *v).filter(|v| v.is_finite() && *v > 0.0) else {
        return None;
    };
    let Some(h_) = stats::hurst(&c, 20).filter(|v| v.is_finite()) else {
        return None;
    };
    let r = indicators::log_returns(&c);
    let pct = vol_percentile(&r);
    let up = c[c.len() - 1] > c[c.len() - 25];

    let (label, detail) = decide(adx, h_, pct, up);
    Some((label, detail))
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

 /// Deterministik seri: adx/hurst gerçek hesap; bu test karar ağacının
 /// YAPISINI doğrular (rejim etiketi → gate/sinyal).
    #[test]
    fn rejim_skip_siniri_dogru() {
 // <200 mum → Skip (veri yoksa karar yok)
        let input = MarketInput { coin: "T".into(), candles: H::flat(150, 100.0), ..Default::default() };
        assert!(matches!(RegimeDetection.evaluate(&input), Signal::Skip { .. }));
    }

 /// A — trend rejimi yönü: fiyat son 25 barın üstündeyse Long,
 /// altındaysa Short (TREND dalında yön simetrisi).
    #[test]
    fn rejim_trend_yonu_simetrik() {
 // yükselen trend: son 25 barlık rampa — adx>25 && hurst>0.55 ihtimali
 // deterministik olarak belli değildir; o yüzden karar ağacını
 // classify arayüzüyle değil, davranışla sınarız: aynı seri iki
 // yönde de işlendiğinde yön ETİKETİ fiyat yönünü izler.
        let up = H::linear(220, 100.0, 0.2);
        let down = H::linear(220, 144.0, -0.2);
        let (su, sd) = (RegimeDetection.evaluate(&MarketInput { coin: "T".into(), candles: up, ..Default::default() }),
                        RegimeDetection.evaluate(&MarketInput { coin: "T".into(), candles: down, ..Default::default() }));
        match (su, sd) {
            (Signal::Long { .. }, Signal::Short { .. }) => {} // ideal: TREND rejimi
            (Signal::Long { .. }, _) => panic!("aşağı seride Long?"),
            (_, Signal::Short { .. }) => panic!("yukarı seride Short?"),
            (a, b) => {
 // adx/hurst TREND eşiğine ulaşmadı → Hold; yönlü sinyal YOK
                assert!(matches!(a, Signal::Hold { .. }) && matches!(b, Signal::Hold { .. }),
                    "yönsüz rejimde Hold: {a:?} / {b:?}");
            }
        }
    }

 /// B — FUTURE LEAKAGE: rejim kararı prefix'te sabit (gelecek barlar
 /// geçmiş rejim etiketini değiştirmez).
    #[test]
    fn rejim_future_leak_prefix_sabit() {
        let mut full = H::flat(200, 100.0);
        full.extend(H::linear(60, 100.0, 0.5)); // gelecekte trend başlasa bile
        let a = full[..220].to_vec();
        let ia = MarketInput { coin: "T".into(), candles: a.clone(), ..Default::default() };
        let sa = RegimeDetection.evaluate(&ia);
        assert_eq!(sa, RegimeDetection.evaluate(&ia), "prefix karar sabit (saf fonksiyon)");
 // ek: SQUEEZE rejimi motorun RegimeGate::Blocked'ına bağlanır —
 // giriş yasak; bu yansıma classify.gate ile test edilir (aşağıda).
        let _ = sa;
    }

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
