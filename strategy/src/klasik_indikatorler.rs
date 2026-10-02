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
        let rsi = indicators::rsi(&c, 14).last().and_then(|v| *v).unwrap_or(50.0);
        if (30.0..50.0).contains(&rsi) { score += 1.0; notes.push(format!("RSI {rsi:.0} dip bölge")); }
        else if rsi > 70.0 { score -= 0.5; notes.push(format!("RSI {rsi:.0} aşırı")); }

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

        // 4) OBV eğimi (son 10 bar)
        let obv = indicators::obv(&c, &vol_of(&input.candles));
        if obv.len() >= 10 && obv[obv.len() - 1] > obv[obv.len() - 10] {
            score += 0.5; notes.push("OBV yukarı".into());
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
