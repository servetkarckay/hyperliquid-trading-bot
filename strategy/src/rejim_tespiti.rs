//! REJİM TESPİTİ — piyasa modunu sınıflandırır; diğer stratejilerin hangi
//! modda çalışması gerektiğini belirleyen filtre katmanı.
//!
//! 3 BOYUT: ADX14 (trend gücü), Hurst üssü (R/S), vol yüzdelik
//! (son 30 barın 200-bar geçmişteki yeri).
//! KARAR AĞACI:
//!   TREND          — ADX>25 + H>0.55 -> yön momentum yönünde Long/Short [0.5]
//!   MEAN_REVERSION — ADX<20 + H<0.45 (sinyal stratejilerini bekler)
//!   VOL_SPIKE      — vol %90 üstü -> pozisyon küçültme
//!   SQUEEZE        — vol %10 altı -> breakout bekleme
//!   GECIS          — diğer durumlar -> nötr
//! GEREKLİ VERİ: >=200 mum.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;
use math::stats;

pub struct RejimTespiti;

impl Strategy for RejimTespiti {
    fn name(&self) -> &'static str {
        "Rejim Tespiti"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 200 {
            return Signal::Skip { reason: ">=200 mum gerekli".into() };
        }
        let h = input.candles.iter().map(|x| x.h).collect::<Vec<_>>();
        let l = input.candles.iter().map(|x| x.l).collect::<Vec<_>>();

        let adx = indicators::adx(&h, &l, &c, 14).2.last().and_then(|v| *v).unwrap_or(0.0);
        let hurst = stats::hurst(&c, 20).unwrap_or(0.5);

        // vol yüzdelik: son 30 bar vol'un son 200 bar içindeki yeri
        let r = indicators::log_returns(&c);
        let vol30 = |end: usize| -> f64 {
            let w = &r[end - 30..end];
            (w.iter().map(|x| x * x).sum::<f64>() / 30.0).sqrt()
        };
        let cur = vol30(r.len());
        let hist: Vec<f64> = (30..r.len()).step_by(10).map(|i| vol30(i)).collect();
        let pct = hist.iter().filter(|v| **v <= cur).count() as f64 / hist.len() as f64;

        // rejim karar ağacı (doküman matrisi)
        let regime = if adx > 25.0 && hurst > 0.55 {
            "TREND"
        } else if adx < 20.0 && hurst < 0.45 {
            "MEAN_REVERSION"
        } else if pct > 0.9 {
            "VOL_SPIKE"
        } else if pct < 0.1 {
            "SQUEEZE"
        } else {
            "GECIS"
        };

        // yön: trend rejiminde EMA eğimi
        let trend_dir = if c[c.len() - 1] > c[c.len() - 25] { 1.0 } else { -1.0 };

        match regime {
            "TREND" => if trend_dir > 0.0 {
                Signal::Long { strength: 0.5, note: format!("TREND rejimi: ADX={adx:.1} H={hurst:.2}") }
            } else {
                Signal::Short { strength: 0.5, note: format!("TREND rejimi aşağı: ADX={adx:.1} H={hurst:.2}") }
            },
            "MEAN_REVERSION" => Signal::Hold { note: format!("MR rejimi: ADX={adx:.1} H={hurst:.2} — s02/s19 aktif olmalı") },
            "SQUEEZE" => Signal::Hold { note: format!("SQUEEZE: vol yüzdelik %{:.0} — s23 breakout beklemede", pct * 100.0) },
            "VOL_SPIKE" => Signal::Hold { note: "VOL_SPIKE: pozisyon küçült (risk katmanı)" .into() },
            _ => Signal::Hold { note: format!("GEÇİŞ rejimi: ADX={adx:.1} H={hurst:.2} vol%{:.0}", pct * 100.0) },
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
pub fn classify(input: &crate::MarketInput) -> Option<(RegimeLabel, String)> {
    use crate::closes;
    let c = closes(&input.candles);
    if c.len() < 200 {
        return None;
    }
    let h: Vec<f64> = input.candles.iter().map(|x| x.h).collect();
    let l: Vec<f64> = input.candles.iter().map(|x| x.l).collect();

    let adx = indicators::adx(&h, &l, &c, 14).2.last().and_then(|v| *v).unwrap_or(0.0);
    let h_ = stats::hurst(&c, 20).unwrap_or(0.5);
    let r = indicators::log_returns(&c);
    let vol30 = |end: usize| -> f64 {
        let w = &r[end - 30..end];
        (w.iter().map(|x| x * x).sum::<f64>() / 30.0).sqrt()
    };
    let cur = vol30(r.len());
    let hist: Vec<f64> = (30..r.len()).step_by(10).map(|i| vol30(i)).collect();
    let pct = hist.iter().filter(|v| **v <= cur).count() as f64 / hist.len() as f64;
    let up = c[c.len() - 1] > c[c.len() - 25];

    let (label, detail) = if adx > 25.0 && h_ > 0.55 {
        (RegimeLabel::Trend { up }, format!("TREND: ADX={adx:.1} H={h_:.2}"))
    } else if adx < 20.0 && h_ < 0.45 {
        (RegimeLabel::MeanReversion, format!("MR: ADX={adx:.1} H={h_:.2}"))
    } else if pct > 0.9 {
        (RegimeLabel::VolSpike, format!("VOL_SPIKE: %{:.0}", pct * 100.0))
    } else if pct < 0.1 {
        (RegimeLabel::Squeeze, format!("SQUEEZE: %{:.0}", pct * 100.0))
    } else {
        (RegimeLabel::Gecis, format!("GEÇİŞ: ADX={adx:.1} H={h_:.2} vol%{:.0}", pct * 100.0))
    };
    Some((label, detail))
}
