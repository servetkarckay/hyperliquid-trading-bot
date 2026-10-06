//! WYCKOFF — arz-talep efor/sonuç divergence (birikim/dağıtım).
//!
//! 1. Girdi: ≥60 mum + hacim; EMA boş/NaN → Skip (panik yok).
//! 2. Trend: EMA20/EMA50 işareti; efor = son bar hacmi / 20-bar ortalama.
//! 3. Sonuç: son 20 barlık aralıkta konum (0=alt, 1=üst).
//! 4. Divergence: trend↑ + hacim onayı + zirveye yakın değil → Long;
//! ayna Short; yeni zirve + hacim kurusu → dağıtım Short [0.3];
//! yeni dip + hacim patlaması → birikim Long [0.5]; karışık → Hold.
use crate::{closes, last, volumes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct Wyckoff;

const VOL_AVG: usize = 20;

impl Strategy for Wyckoff {
    fn name(&self) -> &'static str {
        "Wyckoff"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 60 {
            return Signal::Skip { reason: ">=60 mum gerekli".into() };
        }
        let vols = volumes(&input.candles);
 // D: EMA boş/NaN → Skip (panik yerine; engine zaten panik-izole eder
 // ama deterministik davranış üretimde Skip olmalı)
        let (Some(e20), Some(e50)) = (
            last(&indicators::ema(&c, 20)).filter(|x| x.is_finite()),
            last(&indicators::ema(&c, 50)).filter(|x| x.is_finite()),
        ) else {
            return Signal::Skip { reason: "EMA hesaplanamadı".into() };
        };
        let price = c[c.len() - 1];

 // trend yönü
        let trend = if e20 > e50 { 1.0 } else { -1.0 };

 // efor (hacim) / sonuç (fiyat): son barın hacmi 20-bar ortalamanın kaç katı
        let v_avg: f64 = vols[vols.len() - VOL_AVG..].iter().sum::<f64>() / VOL_AVG as f64;
        let v_last = vols[vols.len() - 1];
        let effort = v_last / v_avg.max(1e-12);

 // 20 barlık aralık pozisyonu (0=alt, 1=üst)
        let hh = highs_of(&input.candles);
        let ll = lows_of(&input.candles);
        let range = (hh - ll).abs().max(1e-12);
        let pos = (price - ll) / range;

 // yeni zirve + hacim kurusu = dağıtım (short); yeni dip + hacim patlaması = birikim (long)
        let at_high = pos > 0.95;
        let at_low = pos < 0.05;
        let dry_up = effort < 0.7;
        let burst = effort > 1.8;

        if trend > 0.0 && !at_high && effort > 1.2 {
            return Signal::Long {
                strength: (0.4 + effort.min(3.0) * 0.1).min(0.9),
                note: format!("trend yukarı (E20>E50), hacim onaylı ({effort:.1}x), aralık konumu {pos:.2}"),
            };
        }
        if trend < 0.0 && !at_low && effort > 1.2 {
            return Signal::Short {
                strength: (0.4 + effort.min(3.0) * 0.1).min(0.9),
                note: format!("trend aşağı, hacim onaylı"),
            };
        }
        if at_high && dry_up {
            return Signal::Short { strength: 0.3, note: "yeni zirve + hacim kurusu (dağıtım)".into() };
        }
        if at_low && burst {
            return Signal::Long { strength: 0.5, note: "yeni dip + hacim patlaması (birikim/spring)".into() };
        }
        Signal::Hold { note: format!("karışık: trend={trend:+.0} effort={effort:.1} pos={pos:.2}") }
    }
}

fn highs_of(candles: &[data::Candle]) -> f64 {
    candles[candles.len() - 20..].iter().map(|c| c.h).fold(f64::MIN, f64::max)
}
fn lows_of(candles: &[data::Candle]) -> f64 {
    candles[candles.len() - 20..].iter().map(|c| c.l).fold(f64::MAX, f64::min)
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

 /// A — Long koşulu: trend yukarı (E20>E50) + hacim onayı + zirvede değil.
    #[test]
    fn wyckoff_trend_up_hacim_onayli_long_uretir() {
        let mut candles = H::linear(80, 100.0, 0.1); // yükselen
        H::set_last(&mut candles, 108.0, 108.5, 107.0, 300.0); // hacim patlaması
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!(strength > 0.0 && strength <= 0.9);
                assert!(note.contains("trend yukarı"));
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

 /// C — Short simetrisi: trend aşağı + hacim onayı.
    #[test]
    fn wyckoff_trend_down_hacim_onayli_short_uretir() {
        let mut candles = H::linear(80, 108.0, -0.1);
        H::set_last(&mut candles, 100.0, 101.0, 99.5, 300.0);
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Short { strength, .. } => assert!(strength > 0.0 && strength <= 0.9),
            other => panic!("Short beklenirdi: {other:?}"),
        }
    }

 /// D — EMA hesaplanamadı → Skip (panik/unwrap yok).
    #[test]
    fn wyckoff_ema_yoksa_skip() {
 // 60 mum ama hepsi aynı — ema yine değer üretir; Skip yolu için
 // NaN mum kullan (ema NaN üretir → filtre Skip'e düşmeli)
        let mut candles = H::flat(80, 100.0);
        H::set_last(&mut candles, f64::NAN, 100.0, 99.0, 100.0);
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        assert!(matches!(Wyckoff.evaluate(&input), Signal::Skip { .. }), "NaN son bar → Skip");
    }

 /// B — FUTURE LEAKAGE: kırılım/karar sinyali onay verisi OLMADAN üretilmez.
 /// Veri A = t anına kadar; t'ten ÖNCE sinyal yok, t'te var.
    #[test]
    fn wyckoff_prefix_replay_erken_sinyal_uretmez() {
        let candles = H::linear(80, 100.0, 0.1);
 // son bar hariç: EMA'lar benzer ama hacim onayı son barda geldi
        let early = candles[..candles.len() - 1].to_vec();
        let input = MarketInput { coin: "T".into(), candles: early, ..Default::default() };
        let sig = Wyckoff.evaluate(&input);
 // erken sinyal hacim patlaması olmadan KONTRAYSA (Short/Hold/Skip) doğru;
 // kritik olan: son barın bilgisine bakmadan Long ÜRETMEZ (hacim onayı yok)
        assert!(
            !matches!(&sig, Signal::Long { note, .. } if note.contains("hacim onaylı")),
            "hacim patlaması olmadan hacim-onaylı Long üretilmez: {sig:?}"
        );
    }
}
