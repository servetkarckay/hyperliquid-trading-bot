//! DOW TEORİSİ — birincil trend yapısı kontrolü (tahmin değil ELEME).
//!
//! 1. Pivotlar: ±10 bar merkezli en yüksek/en düşük (onay ±10 bar gecikir
//!    — sızıntısız, prefix-replay testli).
//! 2. Yapı: HH+HL (yükseliş) / LH+LL (düşüş) + hacim doğrulayıcı
//!    (yükseliş bar hacmi baskınlığı) + EMA50 konumu.
//! 3. Dört onay tamamsa Long/Short [0.6]; eksikte Hold (eleme).
//! 4. EMA hesaplanamazsa Skip — SAHTE onay yok (eski `unwrap_or(0.0)`
//!    sahte Long üretiyordu).
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct DowTeorisi;

impl Strategy for DowTeorisi {
    fn name(&self) -> &'static str {
        "Dow"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 100 {
            return Signal::Skip { reason: ">=100 mum gerekli (swing yapısı)".into() };
        }
        // pivot tespiti: 10 bar sol/sağ en yüksek/en düşük
        let (piv_high, piv_low) = pivots(&c, 10);
        if piv_high.len() < 2 || piv_low.len() < 2 {
            return Signal::Hold { note: "yeterli pivot yok".into() };
        }
        // HH/HL: son iki pivot zirve/dip karşılaştırması
        let hh = piv_high[piv_high.len() - 1] > piv_high[piv_high.len() - 2];
        let hl = piv_low[piv_low.len() - 1] > piv_low[piv_low.len() - 2];
        let lh = piv_high[piv_high.len() - 1] < piv_high[piv_high.len() - 2];
        let ll = piv_low[piv_low.len() - 1] < piv_low[piv_low.len() - 2];

        // hacim doğrulayıcı: yükseliş barlarında hacim > düşüş barlarında mı
        let vols: Vec<f64> = input.candles.iter().map(|x| x.v).collect();
        let up_v: f64 = c.windows(2).enumerate().filter(|(_i, w)| w[1] > w[0]).map(|(i, _)| vols[i + 1]).sum();
        let dn_v: f64 = c.windows(2).enumerate().filter(|(_i, w)| w[1] < w[0]).map(|(i, _)| vols[i + 1]).sum();
        let vol_confirms_up = up_v > dn_v;

        // EMA50 YOK → onay VERİLMEZ (eski `unwrap_or(0.0)` EMA boşsa
        // `px > 0.0` trivially true yapıp SAHTE Long onayı üretiyordu).
        let Some(e50) = last_ema(&c, 50) else {
            return Signal::Skip { reason: "EMA50 hesaplanamadı".into() };
        };

        if hh && hl && vol_confirms_up && c[c.len() - 1] > e50 {
            Signal::Long { strength: 0.6, note: "HH + HL + hacim onayı + E50 üstü (birincil trend yukarı)".into() }
        } else if lh && ll && !vol_confirms_up && c[c.len() - 1] < e50 {
            Signal::Short { strength: 0.6, note: "LH + LL + hacim onayı + E50 altı (birincil trend aşağı)".into() }
        } else {
            Signal::Hold { note: format!("yapı karışık: HH={hh} HL={hl} — Dow eleme: sinyal yok") }
        }
    }
}

fn pivots(c: &[f64], k: usize) -> (Vec<f64>, Vec<f64>) {
    let mut highs = Vec::new();
    let mut lows = Vec::new();
    for i in k..c.len() - k {
        let win = &c[i - k..=i + k];
        if c[i] == win.iter().cloned().fold(f64::MIN, f64::max) {
            highs.push(c[i]);
        }
        if c[i] == win.iter().cloned().fold(f64::MAX, f64::min) {
            lows.push(c[i]);
        }
    }
    (highs, lows)
}

fn last_ema(c: &[f64], n: usize) -> Option<f64> {
    indicators::ema(c, n).last().and_then(|x| *x).filter(|x| x.is_finite() && *x > 0.0)
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

    /// Deterministik zigzag: pivotlar (bar, fiyat) arasında lineer interpolasyon.
    /// Yükselen ayaklarda hacim 250, düşenlerde 50.
    fn zigzag(pivots: &[(usize, f64)], n: usize, up_v: f64, down_v: f64) -> Vec<data::Candle> {
        let mut candles = Vec::new();
        for i in 0..n {
            // pivot segmentini bul
            let mut seg = 0usize;
            for (j, (b, _)) in pivots.iter().enumerate() {
                if *b <= i {
                    seg = j;
                }
            }
            let (b0, p0) = pivots[seg];
            let (b1, p1) = pivots[(seg + 1).min(pivots.len() - 1)];
            let px = if b1 > b0 {
                p0 + (p1 - p0) * (i - b0) as f64 / (b1 - b0) as f64
            } else {
                p0
            };
            let up = p1 >= p0;
            let v = if up { up_v } else { down_v };
            candles.push(H::candle(i as i64 * 3_600_000, px, px + 0.05, px - 0.05, px, v));
        }
        candles
    }

    /// Yükseliş yapısı: HH (112→122) + HL (98→102→108), yükseliş hacmi baskın.
    #[test]
    fn dow_hh_hl_hacim_e50_long_uretir() {
        let pivs = [(0usize, 100.0), (10, 98.0), (30, 112.0), (50, 102.0), (70, 122.0), (90, 108.0), (119, 116.0)];
        let candles = zigzag(&pivs, 120, 250.0, 50.0);
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        match DowTeorisi.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!(strength > 0.0);
                assert!(note.contains("E50"), "nota onay koşulu yazılır: {note}");
            }
            other => panic!("HH+HL Long beklenirdi: {other:?}"),
        }
    }

    /// C — Short simetrisi: LH (122→118→112) + LL (108→98), düşüş hacmi baskın.
    #[test]
    fn dow_lh_ll_short_uretir() {
        let pivs = [(0usize, 112.0), (10, 122.0), (30, 108.0), (50, 118.0), (70, 98.0), (90, 112.0), (119, 104.0)];
        // düşen ayaklarda hacim baskın (dow short hacim onayı)
        let candles = zigzag(&pivs, 120, 50.0, 250.0);
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        match DowTeorisi.evaluate(&input) {
            Signal::Short { strength, .. } => assert!(strength > 0.0),
            other => panic!("LH+LL Short beklenirdi: {other:?}"),
        }
    }

    /// D — EMA hesaplanamazsa Skip: SAHTE Long onayı ÜRETİLMEZ
    /// (eski `unwrap_or(0.0)` EMA boşsa `px > 0.0` trivially true yapıyordu).
    #[test]
    fn dow_ema_yoksa_skip_sahte_onay_uretmez() {
        let pivs = [(0usize, 100.0), (10, 98.0), (30, 112.0), (50, 102.0), (70, 122.0), (90, 108.0), (119, 116.0)];
        let mut c = zigzag(&pivs, 120, 250.0, 50.0);
        H::set(&mut c, 119, f64::NAN, f64::NAN, f64::NAN, 100.0);
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        assert!(matches!(DowTeorisi.evaluate(&input), Signal::Skip { .. }));
    }

    /// B — FUTURE LEAKAGE: pivot onayı ±10 bar pencere gerektirir.
    /// Son pivot bar 70'te → 80'e kadar yapısı BİLİNEMEZ; bar 79'daki
    /// prefix yön sinyali ÜRETMEZ (erken sinyal = sızıntı).
    /// + Gelecek 50 bar eklenmiş serinin AYNI prefix'i AYNI sinyali verir.
    #[test]
    fn dow_future_leak_pivot_onayindan_once_sinyal_yok() {
        let pivs = [(0usize, 100.0), (10, 98.0), (30, 112.0), (50, 102.0), (70, 122.0), (90, 108.0), (119, 116.0)];
        let full = zigzag(&pivs, 170, 250.0, 50.0);
        // A = bar 0..=79 (son pivot penceresi 60..80 tamamlanmamış)
        let a = full[..80].to_vec();
        let input = MarketInput { coin: "T".into(), candles: a, ..Default::default() };
        let sig_a = DowTeorisi.evaluate(&input);
        assert!(
            !matches!(sig_a, Signal::Long { .. } | Signal::Short { .. }),
            "pivot onayı tamamlanmadan yön sinyali ÜRETİLMEZ: {sig_a:?}"
        );
        // B = A + 50 gelecek bar; t anındaki sinyal (B[0..80]) A ile aynı
        let input_b = MarketInput { coin: "T".into(), candles: full[..80].to_vec(), ..Default::default() };
        assert_eq!(sig_a, DowTeorisi.evaluate(&input_b), "gelecek barlar geçmiş kararını değiştirmez");
        // pivot penceresi tamamlandığında sinyal GELİR (gerekçe kanıtlandı)
        let full2 = zigzag(&pivs, 120, 250.0, 50.0);
        let input2 = MarketInput { coin: "T".into(), candles: full2, ..Default::default() };
        assert!(matches!(DowTeorisi.evaluate(&input2), Signal::Long { .. }));
    }
}
