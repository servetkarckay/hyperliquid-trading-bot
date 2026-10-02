//! DOW TEORİSİ — birincil trend yapısı kontrolü (tahmin değil ELEME).
//!
//! HESAPLAMA:
//! 1. Pivotlar: ±10 bar merkezi en yüksek/en düşük (prime swings)
//! 2. Yapı: son iki pivot -> HH+HL (yükseliş) veya LH+LL (düşüş)
//! 3. Hacim doğrulayıcı: yükseliş barlarının hacmi > düşüşlerin mi
//! 4. EMA50 konumu (birincil trend teyidi)
//! 5. Karar: HH+HL+hacim onayı+E50 üstü -> Long [0.6]; ayna Short;
//!    eksikte Hold (eleme — yapı bozuksa sinyal yok).
//! GEREKLİ VERİ: >=100 mum + hacim.
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

        let e50 = last_ema(&c, 50);

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

fn last_ema(c: &[f64], n: usize) -> f64 {
    indicators::ema(c, n).last().and_then(|x| *x).unwrap_or(0.0)
}
