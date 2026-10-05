//! KLASİK GÖSTERGELER — 5 göstergeli birleşik skor.
//!
//! KATMANLAR (her biri oy verir):
//!   RSI14     — 30-50 dip bölge: +1; >70: -0.5 (aşırı — tuzak kuralı)
//!   MACD hist — işaret ±1
//!   Bollinger — alt banda temas +1; üst banda -1
//!   OBV       — son 10 bar artışsa +0.5
//!   VWAP20    — fiyat üstünde +0.5; altında -0.5
//! SKOR: >=2 -> Long, <=-2 -> Short [0-0.8]; yoksa Hold.
//! KLAVUZ: "RSI>70 -> SHORT" içiçe tuzak KULLANILMAZ; gösterge
//! konum + hacim onayıyle birlikte oylar.
//! GEREKLİ VERİ: >=60 mum.
use crate::{closes, last, MarketInput, Signal, Strategy};
use math::indicators;

pub struct KlasikIndikatorler;

impl Strategy for KlasikIndikatorler {
    fn name(&self) -> &'static str {
        "Klasik Göstergeler"
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
        // I — SİMETRİ DÜZELTMESİ: dip alım ödülü (+1) ile aşırı alım cezası
        // eşit ağırlıkta (eski kod: aşırı alım −0.5, OBV aşağı kolü YOK →
        // Short skoru yapısal ulaşılmazdı = gizli Long yanlılığı).
        let rsi = indicators::rsi(&c, 14).last().and_then(|v| *v).unwrap_or(50.0);
        if (30.0..50.0).contains(&rsi) { score += 1.0; notes.push(format!("RSI {rsi:.0} dip bölge")); }
        else if rsi > 70.0 { score -= 1.0; notes.push(format!("RSI {rsi:.0} aşırı")); }

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
        if let (Some(_), Some(up), Some(lo)) = bb.last().copied().unwrap() {
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
    fn sine(n: usize, period: f64, amp: f64, v: f64) -> Vec<data::Candle> {
        (0..n)
            .map(|i| {
                let px = 100.0 + amp * (2.0 * PI * i as f64 / period).sin();
                H::candle(i as i64 * 3_600_000, px, px + 0.05, px - 0.05, px, v)
            })
            .collect()
    }

    /// A — dip noktasında çoklu onay → Long (skor ≥ +2).
    #[test]
    fn klasik_coklu_onay_long() {
        let full = sine(200, 60.0, 2.0, 150.0);
        // sinüsün DİP noktası: i = 3T/4 = 45 (i mod 60)
        let mut best: Option<(usize, String)> = None;
        for cut in 150..=190 {
            let input = MarketInput { coin: "T".into(), candles: full[..cut].to_vec(), ..Default::default() };
            if let Signal::Long { strength, note } = KlasikIndikatorler.evaluate(&input) {
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
        let full = sine(200, 60.0, 2.0, 150.0);
        // zirve + DÖNÜŞ sonrası kısa inişte MACD hist − olur; zirve barında
        // hist hâlâ + olabilir → dönüşü izleyen barlarda ara
        let mut best: Option<(usize, String)> = None;
        for cut in 150..=190 {
            let input = MarketInput { coin: "T".into(), candles: full[..cut].to_vec(), ..Default::default() };
            if let Signal::Short { strength, note } = KlasikIndikatorler.evaluate(&input) {
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
        assert!(matches!(KlasikIndikatorler.evaluate(&input), Signal::Skip { .. }));
    }
}
