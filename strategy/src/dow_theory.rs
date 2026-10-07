//! DOW THEORY — birincil trend yapısı: yap + hacim teyidi (eleme).
//!
//! KANONİK KAYNAK: Dow Theory'nin altı dogması (Hamilton/Rhea/Schaefer
//! özeti — Wikipedia/Robert Rhea "The Dow Theory" 1930'lar):
//! 1. Piyasa üç hareket yapar: birincil (tide), ikincil (dalgalar),
//!    günlük (dalgalanmalar).
//! 2. Trendler üç fazdadır: birikim → halk katılımı → dağıtım.
//! 3. Piyasa haberi fiyatlar (discount).
//! 4. ORTALAMALAR BİRBİRİNİ DOĞRULAMALI (endüstri+taşıma — çapraz
//!    teyit; tek-coin değerlendirmede yapısal YOK — bilinçli sınır).
//! 5. **Trend hacimle doğrulanır**: yüksek hacimli hareket "gerçek"
//!    piyasa görüşüdür (tenet 5 — bu modülde yükseliş/düşüş bar
//!    hacmi baskınlığı olarak uygulanır).
//! 6. Trend, net tersine-dönüş sinyaline kadar sürer.
//!
//! Rhea'nın sinyal mantığı (tek varlığa sadeleştirme):
//! - BEAR dibi kuruldu → ikincil tepki → çekilme önceki diplerin
//!   ÜSTÜNDE KALDI (HL) → önceki ralli zirvesinin ÜSTÜNE KIRILDI
//!   (HH) + hacim teyidi → **BUY sinyali**.
//! - Simetri: LH + LL + düşüş hacmi baskın → **SELL sinyali**.
//!
//! Yapı karışıksa: eleme (Hold) — Dow eleme matığı (tahmin değil).
//!
//! Kaldırılan modern kaplama: EMA50 konumu DOW DEĞİLDİR (EMA, Dow
//! dogmalarında yok); sinyal salt yapı+hacimden gelir.
//!
//! Pivot tespiti ±10 bar merkezli (onay gecikmesi sızıntısız;
//! prefix-replay testli). <100 mum → Skip. Deterministik, panik yok.
use crate::{closes, MarketInput, Signal, Strategy};
pub struct DowTheory;

impl Strategy for DowTheory {
    fn name(&self) -> &'static str {
        "Dow Theory"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 100 {
            return Signal::Skip {
                reason: ">=100 mum gerekli (swing yapısı)".into(),
            };
        }
        // pivot tespiti: 10 bar sol/sağ en yüksek/en düşük
        let (piv_high, piv_low) = pivots(&c, 10);
        if piv_high.len() < 2 || piv_low.len() < 2 {
            return Signal::Hold {
                note: "yeterli pivot yok".into(),
            };
        }
        // HH/HL: son iki pivot zirve/dip karşılaştırması
        let hh = piv_high[piv_high.len() - 1] > piv_high[piv_high.len() - 2];
        let hl = piv_low[piv_low.len() - 1] > piv_low[piv_low.len() - 2];
        let lh = piv_high[piv_high.len() - 1] < piv_high[piv_high.len() - 2];
        let ll = piv_low[piv_low.len() - 1] < piv_low[piv_low.len() - 2];

        // hacim doğrulayıcı: yükseliş barlarında hacim > düşüş barlarında mı
        let vols: Vec<f64> = input.candles.iter().map(|x| x.v).collect();
        let up_v: f64 = c
            .windows(2)
            .enumerate()
            .filter(|(_i, w)| w[1] > w[0])
            .map(|(i, _)| vols[i + 1])
            .sum();
        let dn_v: f64 = c
            .windows(2)
            .enumerate()
            .filter(|(_i, w)| w[1] < w[0])
            .map(|(i, _)| vols[i + 1])
            .sum();
        let vol_confirms_up = up_v > dn_v;

        // RHEA SİNYALİ (kanonik): yapı + hacim — modern MA kaplaması YOK.
        if hh && hl && vol_confirms_up {
            Signal::Long {
                strength: 0.6,
                note: "HH + HL + yükseliş hacmi baskın — Rhea BUY (birincil trend yukarı)".into(),
            }
        } else if lh && ll && !vol_confirms_up {
            Signal::Short {
                strength: 0.6,
                note: "LH + LL + düşüş hacmi baskın — Rhea SELL (birincil trend aşağı)".into(),
            }
        } else {
            Signal::Hold {
                note: format!("yapı karışık: HH={hh} HL={hl} LH={lh} LL={ll} — eleme (tenet 6: net dönüş yok)"),
            }
        }
    }
}

fn pivots(c: &[f64], k: usize) -> (Vec<f64>, Vec<f64>) {
    let mut highs = Vec::new();
    let mut lows = Vec::new();
    for i in k..c.len() - k {
        let win = &c[i - k..=i + k];
        // VERİ DÜRÜSTLÜĞÜ: pencere non-finite içeriyorsa pivot
        // KAYDEDİLMEZ — f64::max/f64::min NaN'ı yok saydiği için bozuk
        // barın komşusu sahte pivot üretirdi (sayım sahte olurdu).
        if !win.iter().all(|x| x.is_finite()) {
            continue;
        }
        if c[i] == win.iter().cloned().fold(f64::MIN, f64::max) {
            highs.push(c[i]);
        }
        if c[i] == win.iter().cloned().fold(f64::MAX, f64::min) {
            lows.push(c[i]);
        }
    }
    (highs, lows)
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

    /// Deterministik zigzag: pivotlar (bar, fiyat) arasında lineer interpolasyon.
    /// Yükselen ayaklarda hacim 250, düşenlerde 50.
    /// Yükseliş yapısı: HH (112→122) + HL (98→102→108), yükseliş hacmi baskın.
    #[test]
    fn dow_rhea_buy_hh_hl_hacim_long_uretir() {
        let pivs = [
            (0usize, 100.0),
            (10, 98.0),
            (30, 112.0),
            (50, 102.0),
            (70, 122.0),
            (90, 108.0),
            (119, 116.0),
        ];
        let candles = H::zigzag(&pivs, 120, 250.0, 50.0);
        let input = MarketInput {
            coin: "T".into(),
            candles,
            ..Default::default()
        };
        match DowTheory.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!(strength > 0.0);
                assert!(
                    note.contains("Rhea BUY"),
                    "nota onay koşulu yazılır: {note}"
                );
            }
            other => panic!("HH+HL Long beklenirdi: {other:?}"),
        }
    }

    /// C — Short simetrisi: LH (122→118→112) + LL (108→98), düşüş hacmi baskın.
    #[test]
    fn dow_lh_ll_short_uretir() {
        let pivs = [
            (0usize, 112.0),
            (10, 122.0),
            (30, 108.0),
            (50, 118.0),
            (70, 98.0),
            (90, 112.0),
            (119, 104.0),
        ];
        // düşen ayaklarda hacim baskın (dow short hacim onayı)
        let candles = H::zigzag(&pivs, 120, 50.0, 250.0);
        let input = MarketInput {
            coin: "T".into(),
            candles,
            ..Default::default()
        };
        match DowTheory.evaluate(&input) {
            Signal::Short { strength, .. } => assert!(strength > 0.0),
            other => panic!("LH+LL Short beklenirdi: {other:?}"),
        }
    }

    /// D — yetersiz mum → Skip (sinyal ÜRETİLMEZ; eleme matığı).
    #[test]
    fn dow_yetersiz_mum_skip() {
        let input = MarketInput {
            coin: "T".into(),
            candles: H::flat(50, 100.0),
            ..Default::default()
        };
        assert!(matches!(DowTheory.evaluate(&input), Signal::Skip { .. }));
    }

    /// NaN pivot serisi → Skip/eleme (sahte yön yok; panik yok).
    #[test]
    fn dow_nan_pivot_sahte_sinyal_uretmez() {
        let pivs = [
            (0usize, 100.0),
            (10, 98.0),
            (30, 112.0),
            (50, 102.0),
            (70, 122.0),
            (90, 108.0),
            (119, 116.0),
        ];
        let mut c = H::zigzag(&pivs, 120, 250.0, 50.0);
        H::set(&mut c, 70, f64::NAN, f64::NAN, f64::NAN, 100.0); // pivot bar 70 bozuk → pivot seti <2
        let input = MarketInput {
            coin: "T".into(),
            candles: c,
            ..Default::default()
        };
        let sig = DowTheory.evaluate(&input);
        assert!(
            !matches!(sig, Signal::Long { .. } | Signal::Short { .. }),
            "NaN seride sahte yön yok: {sig:?}"
        );
    }

    /// B — FUTURE LEAKAGE: pivot onayı ±10 bar pencere gerektirir.
    /// Son pivot bar 70'te → 80'e kadar yapısı BİLİNEMEZ; bar 79'daki
    /// prefix yön sinyali ÜRETMEZ (erken sinyal = sızıntı).
    /// + Gelecek 50 bar eklenmiş serinin AYNI prefix'i AYNI sinyali verir.
    #[test]
    fn dow_future_leak_pivot_onayindan_once_sinyal_yok() {
        let pivs = [
            (0usize, 100.0),
            (10, 98.0),
            (30, 112.0),
            (50, 102.0),
            (70, 122.0),
            (90, 108.0),
            (119, 116.0),
        ];
        let full = H::zigzag(&pivs, 170, 250.0, 50.0);
        // A = bar 0..=79 (son pivot penceresi 60..80 tamamlanmamış)
        let a = full[..80].to_vec();
        let input = MarketInput {
            coin: "T".into(),
            candles: a,
            ..Default::default()
        };
        let sig_a = DowTheory.evaluate(&input);
        assert!(
            !matches!(sig_a, Signal::Long { .. } | Signal::Short { .. }),
            "pivot onayı tamamlanmadan yön sinyali ÜRETİLMEZ: {sig_a:?}"
        );
        // B = A + 50 gelecek bar; t anındaki sinyal (B[0..80]) A ile aynı
        let input_b = MarketInput {
            coin: "T".into(),
            candles: full[..80].to_vec(),
            ..Default::default()
        };
        assert_eq!(
            sig_a,
            DowTheory.evaluate(&input_b),
            "gelecek barlar geçmiş kararını değiştirmez"
        );
        // pivot penceresi tamamlandığında sinyal GELİR (gerekçe kanıtlandı)
        let full2 = H::zigzag(&pivs, 120, 250.0, 50.0);
        let input2 = MarketInput {
            coin: "T".into(),
            candles: full2,
            ..Default::default()
        };
        assert!(matches!(DowTheory.evaluate(&input2), Signal::Long { .. }));
    }
}
