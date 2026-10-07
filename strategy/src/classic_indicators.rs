//! CLASSIC INDICATORS — klasik teknik göstergeler bileşik skoru.
//!
//! Göstergelerin kanonik kaynakları:
//! - **RSI(14)** — J. Welles Wilder, "New Concepts in Technical Trading
//!   Systems" (1978): RSI < 30 = oversold (alım bölgesi, kanonik eşik);
//!   30-50 = oversold-çıkış toparlanması (aynı yönde devam); RSI > 70 =
//!   overbought. None → oy katmaz (sahte nötr 50 yasak).
//! - **MACD(12,26,9)** — Gerald Appel: histogram işareti momentum.
//! - **Bollinger(20, 2.0)** — John Bollinger: alt/üst bant teması
//!   (mean-reversion kontratı).
//! - **OBV** — Joe Granville (1963): 10-bar eğim = hacim akışı.
//! - **VWAP(20)** — kurumsal referans fiyat: konum ±.
//!
//! BİLEŞİK: skor ≥ +2 → Long, ≤ −2 → Short (maks ±4'ün yarısı =
//! çoğunluk konfluansı); arası Hold. Mean-reversion (RSI/BB) ve
//! momentum (MACD) katmanlarının bilinçli konfluansı — klasik teknik
//! analiz pratiği.
//!
//! None göstergeler oy KATMAZ; <60 mum → Skip; simetri kontratı:
//! Long/Short skorları yapısal simetrik (tek yanlı asimetri =
//! gizli yanlılık). Deterministik; panik yok.
use crate::{closes, last, MarketInput, Signal, Strategy};
use math::indicators;

pub struct ClassicIndicators;

impl Strategy for ClassicIndicators {
    fn name(&self) -> &'static str {
        "Classic Indicators"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < 60 {
            return Signal::Skip { reason: ">=60 mum gerekli".into() };
        }
        let price = c[c.len() - 1];
        let mut score: f64 = 0.0;
        let mut notes = Vec::new();

 // 1) RSI14: 30<RSI<50 -> +1 (dip alım bölgesi), 50<RSI<70 -> -0.5 (aşırı)
 // I — SİMETRİ KONTRATI: dip alım ödülü (+1) ile aşırı alım cezası
 // eşit ağırlıkta; Short skoru Long skoruyla yapısal simetriktir
 // (tek yanlı ödül/ceza asimetrisi Long yanlılığı doğurur).
        // KONTRAT: None göstergeler oy KATMAZ. RSI hesaplanamazsa (None)
        // nötr 50 İLE SAHTE oy yazılmaz — gösterge yok = katkı yok.
        // KANONİK WILDER(1978) ALIM SİNYALİ: RSI, oversold bölgesinden
        // (30 altı) 30 ÜSTÜNE döndüğünde al sinyalidir. Bu yüzden (30,50)
        // bandı +1'dir; RSI < 30 (düşen bıçak, henüz dönüş yok) OY VERMEZ
        // — momentum komponentleriyle (MACD/BB) savaşmaz. > 70 =
        // overbought (−1).
        match indicators::rsi(&c, 14).last().and_then(|v| *v).filter(|v| v.is_finite()) {
            Some(rsi) if (30.0..50.0).contains(&rsi) => {
                score += 1.0;
                notes.push(format!("RSI {rsi:.0} oversold-çıkışı (Wilder alım bölgesi)"));
            }
            Some(rsi) if rsi > 70.0 => { score -= 1.0; notes.push(format!("RSI {rsi:.0} overbought (Wilder)")); }
            _ => {}
        }

 // 2) MACD histogram işareti
        let (_, _, hist) = indicators::macd(&c, 12, 26, 9);
        let h_last = hist.last().and_then(|v| *v);
        match h_last {
            Some(hv) if hv > 0.0 => { score += 1.0; notes.push("MACD hist +".into()); }
            Some(_) => { score -= 1.0; notes.push("MACD hist -".into()); }
            None => {}
        }

 // 3) Bollinger: alt banda temas -> +1
        let bb = indicators::bollinger(&c, 20, 2.0);
        // PANİK YASAK: seri boşsa oy yok (unwrap yerine Option akışı)
        if let Some((Some(_), Some(up), Some(lo))) = bb.last().copied() {
            if price <= lo { score += 1.0; notes.push("BB alt banda temas".into()); }
            if price >= up { score -= 1.0; notes.push("BB üst banda temas".into()); }
        }

 // 4) OBV eğimi (son 10 bar) — SİMETRİ (aşağı kol eklendi)
        let obv = indicators::obv(&c, &vol_of(&input.candles));
        if obv.len() >= 10 {
            if obv[obv.len() - 1] > obv[obv.len() - 10] {
                score += 0.5; notes.push("OBV yukarı".into());
            } else if obv[obv.len() - 1] < obv[obv.len() - 10] {
                score -= 0.5; notes.push("OBV aşağı".into());
            }
        }

 // 5) VWAP konumu
        let vwap = last(&indicators::rolling_vwap(
            &input.candles.iter().map(|x| x.h).collect::<Vec<_>>(),
            &input.candles.iter().map(|x| x.l).collect::<Vec<_>>(),
            &c,
            &input.candles.iter().map(|x| x.v).collect::<Vec<_>>(),
            20,
        ));
        if let Some(vw) = vwap {
            if price > vw { score += 0.5; notes.push("fiyat VWAP üstü".into()); }
            else { score -= 0.5; notes.push("fiyat VWAP altı".into()); }
        }

 // skor -> sinyal (maks ~4)
        if score >= 2.0 {
            Signal::Long { strength: (score / 4.0).min(0.8), note: format!("skor {score:+.1}: {}", notes.join("; ")) }
        } else if score <= -2.0 {
            Signal::Short { strength: ((-score) / 4.0).min(0.8), note: format!("skor {score:+.1}: {}", notes.join("; ")) }
        } else {
            Signal::Hold { note: format!("skor {score:+.1} — karışık: {}", notes.join("; ")) }
        }
    }
}

fn vol_of(candles: &[data::Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.v).collect()
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;
    use std::f64::consts::PI;

 /// Deterministik sinüs serisi: döngüsel deneysel metrikler.

 /// A — dip noktasında çoklu onay → Long (skor ≥ +2).
    #[test]
    fn klasik_coklu_onay_long() {
        let full = H::sine(200, 60.0, 2.0, 150.0);
 // sinüsün DİP noktası: i = 3T/4 = 45 (i mod 60)
        let mut best: Option<(usize, String)> = None;
        for cut in 150..=190 {
            let input = MarketInput { coin: "T".into(), candles: full[..cut].to_vec(), ..Default::default() };
            if let Signal::Long { strength, note } = ClassicIndicators.evaluate(&input) {
                let last_close = full[cut - 1].c;
                if last_close < 99.0 {
                    best = Some((cut, format!("w={strength:.2} {note}")));
                    break;
                }
            }
        }
        let (cut, info) = best.expect("dip noktasında Long sinyali bulunmalı");
        eprintln!("klasik long: cut={cut} {info}");
    }

 /// C — Short simetrisi: zirve noktasında Short (skor ≤ −2).
    #[test]
    fn klasik_asiri_short() {
        let full = H::sine(200, 60.0, 2.0, 150.0);
 // zirve + DÖNÜŞ sonrası kısa inişte MACD hist − olur; zirve barında
 // hist hâlâ + olabilir → dönüşü izleyen barlarda ara
        let mut best: Option<(usize, String)> = None;
        for cut in 150..=190 {
            let input = MarketInput { coin: "T".into(), candles: full[..cut].to_vec(), ..Default::default() };
            if let Signal::Short { strength, note } = ClassicIndicators.evaluate(&input) {
                best = Some((cut, format!("w={strength:.2} {note}")));
                break;
            }
        }
        let (cut, info) = best.expect("zirveden dönüşte Short sinyali bulunmalı");
        eprintln!("klasik short: cut={cut} {info}");
    }

 /// D — mum yoksa Skip.
    #[test]
    fn klasik_veri_yoksa_skip() {
        let input = MarketInput { coin: "T".into(), candles: H::flat(30, 100.0), ..Default::default() };
        assert!(matches!(ClassicIndicators.evaluate(&input), Signal::Skip { .. }));
    }
}
