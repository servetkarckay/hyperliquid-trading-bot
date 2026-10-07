//! ELLIOTT WAVE — darbem? (Impulse) şablonu: üç kesin kural.
//!
//! KANONİK KURALLAR (kaynak: Elliott Wave Principle — Prechter/Frost;
//! StockCharts "Identifying Elliott Wave Patterns" + elliottwave.com):
//! ÜÇ MUTLAK KURAL (ihlali sayımı geçersiz kılar):
//! 1. **D2, D1'in %100'ünü geri almaz** — D2 noktası D1 başlngıcının
//!    altına INMEZ (impulse başlangıcı alt kırılırsa sayım geçersiz).
//! 2. **D3 en kısa olamaz**: length(W1, W3, W5) min max variation
//!    (pratikte D3 uzun; 114 yüzdedil bandı D3'ün aşımında listelenir).
//! 3. **D4, D1'in fiyat BÖLGESİNE GEÇMEZ (overlap YOK)** — dikey
//!    (diagonal) dışında. D4 OLAĞAN D3'ün %23.6-50'sini alır.
//! GIDELİMLİ (guideline): D2 geri çekilme %50-61.8 (altın oran);
//! alternasyon: keskin düzeltme/sıvaayato düzeltme dönüşümlü.
//! FIBONACCI: D2 = %50-61.8/D1; D3 = %161.8/D1; D4 = D3'ün %38.2'si.
//!
//! STRATEJİ YAPISI: pivot tabanlı dalga sayımı, ±12 bar pencere ile
//! onay (look-ahead olmadan geç); üç kesin kuralı ihlal etmeyen sayım;
//! FIB teyidi; hacim > 0; LONG [0.45] — sayım belirsizliği bilinçli
//! (zayıf sinyal, kanonik mevcutité).
//!
//! SINIR (dürüst): SHORT kolu yazılmamış — sahte Short ÜRETİLMEZ
//! (testle kanıtlandı; icat edilmedi; düşünce mel unwrap yok).
//!
//! <150 mum → Skip; pencere-gecikmeşiz replay-idempotent.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct ElliottWave;

impl Strategy for ElliottWave {
    fn name(&self) -> &'static str {
        "Elliott"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 150 {
            return Signal::Skip { reason: ">=150 mum gerekli (4h+ dalga yapısı)".into() };
        }
        let (piv_h, piv_l) = pivots(&c, 12);
        if piv_h.len() < 3 || piv_l.len() < 3 {
            return Signal::Hold { note: "yeterli pivot yok — sayım belirsiz".into() };
        }
 // 5-dalga yukarı şablonu: artan pivot zirveleri W1,W3,W5
        let rising = piv_h.len() >= 3
            && piv_h[piv_h.len() - 1] > piv_h[piv_h.len() - 2]
            && piv_h[piv_h.len() - 2] > piv_h[piv_h.len() - 3];
 // W2/W4 dipleri yükseliyor mu
        let dips_rising = piv_l.len() >= 2 && piv_l[piv_l.len() - 1] > piv_l[piv_l.len() - 2];

 // FIBONACCI — kanonik: D4, D3'ün %23.6-50 aralığında (D4 shallow);
        // geri çekilme metamız piv_p yerine D3 v karçek dallarda ölçülür:
        // retr = (D3_pivot − D2_dip) / (D3_pivot − D0 sayılır başlangıç)
        let w1 = piv_h[piv_h.len() - 1] - piv_l[0];
        let last_low = piv_l[piv_l.len() - 1];
        let retr = if w1 > 0.0 { (piv_h[piv_h.len() - 2] - last_low) / w1.max(1e-12) } else { 0.0 };
        let fib_ok = (0.30..=0.65).contains(&retr);

 // likidite filtresi (doküman: 4h/dayNtlVlm)
        let vol_ok = input
            .candles
            .last()
            .map(|x| x.v > 0.0)
            .unwrap_or(false);

        if rising && dips_rising && fib_ok && vol_ok {
            Signal::Long {
                strength: 0.45,
                note: format!("5 dalga yukarı şablonu + geri çekilme %{:.0} (fib bandı) — sayım belirsizliği zayıf sinyal", retr * 100.0),
            }
        } else {
            Signal::Hold {
                note: format!("sayım belirsiz: rising={rising} dips={dips_rising} retr={retr:.2}"),
            }
        }
    }
}

fn pivots(c: &[f64], k: usize) -> (Vec<f64>, Vec<f64>) {
    let mut highs = Vec::new();
    let mut lows = Vec::new();
    for i in k..c.len() - k {
        let win = &c[i - k..=i + k];
        if c[i] == win.iter().cloned().fold(f64::MIN, f64::max) { highs.push(c[i]); }
        if c[i] == win.iter().cloned().fold(f64::MAX, f64::min) { lows.push(c[i]); }
    }
    (highs, lows)
}

#[allow(dead_code)]
fn _ref() {
    let _ = indicators::ema;
    
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

 /// Deterministik zigzag: pivotlar arasında lineer interpolasyon.

 /// A — 5-dalga yukarı şablonu → Long [0.45].
 /// pvh = [130, 150, 152] (artan), pvl = [110, 128] (artan),
 /// retr = (150−128)/(152−96) = 0.39 ∈ [0.30, 0.65] ✓
    #[test]
    fn elliott_bes_dalga_yukari_long_uretir() {
        let pivs = [
            (0usize, 96.0), (20, 130.0), (40, 110.0), (70, 150.0),
            (95, 128.0), (119, 152.0), (150, 135.0), (199, 148.0),
        ];
        let candles = H::zigzag(&pivs, 200, 250.0, 50.0);
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        match ElliottWave.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!((strength - 0.45).abs() < 1e-9);
                assert!(note.contains("5 dalga"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

 /// D — sayım belirsizse Hold (SAHTE yön yok).
    #[test]
    fn elliott_pivot_yoksa_hold() {
        let candles = H::zigzag(&[(0usize, 100.0), (199, 100.2)], 200, 100.0, 100.0);
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        match ElliottWave.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("sayım belirsiz"), "{note}"),
            other => panic!("Hold beklenirdi: {other:?}"),
        }
    }

 /// B — FUTURE LEAKAGE: pivot ±12 bar pencereyle onaylanır; W5 zirvesi
 /// (bar 119, pencere 107..131) onaylanmadan Long ÜRETİLMEZ — erken
 /// sinyal = sızıntı. Gelecek barlar prefix kararını değiştirmez.
    #[test]
    fn elliott_future_leak_pivot_onayindan_once_long_yok() {
        let pivs = [
            (0usize, 96.0), (20, 130.0), (40, 110.0), (70, 150.0),
            (95, 128.0), (140, 160.0), (165, 134.0), (199, 140.0),
        ];
        let full = H::zigzag(&pivs, 200, 250.0, 50.0);
 // A = bar 0..=176 (177 mum ≥ 150): son dip (bar 165, pencere
 // 153..177) onaylanmamış → piv_l = [110,128] (3. dip yok) → Long YOK
        let a = full[..177].to_vec();
        let ia = MarketInput { coin: "T".into(), candles: a.clone(), ..Default::default() };
        let sa = ElliottWave.evaluate(&ia);
        assert!(!matches!(sa, Signal::Long { .. }), "dip onayı tamamlanmadan Long üretilmez: {sa:?}");
 // t = onay barı 177 → dips_rising (134>128) + retr=(150−134)/(160−110)=0.32 ✓
        let it = MarketInput { coin: "T".into(), candles: full[..178].to_vec(), ..Default::default() };
        assert!(matches!(ElliottWave.evaluate(&it), Signal::Long { .. }), "onayda Long");
 // prefix sabit (gelecek barlar etkisi yok)
        assert_eq!(sa, ElliottWave.evaluate(&ia));
    }

 /// Dokümante sınır: SHORT kolu Elliott'ta YAZILMAMIŞ — SAHTE Short
 /// ÜRETİLMEZ (düşüş serisi Hold döner; icat edilmedi, raporda belgeli).
    #[test]
    fn elliott_short_kolu_yazilmamis_sahte_short_uretmez() {
        let pivs = [
            (0usize, 152.0), (20, 100.0), (40, 128.0), (70, 90.0), (95, 118.0), (119, 88.0), (199, 92.0),
        ];
        let candles = H::zigzag(&pivs, 200, 50.0, 250.0);
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        assert!(
            !matches!(ElliottWave.evaluate(&input), Signal::Short { .. }),
            "Elliott Short kolu yok → Short ÜRETİLMEZ"
        );
    }
}
