//! TURTLE TREND FOLLOWING — Donchian 20/55 + ATR birim boyutlandırma.
//!
//! KURALLAR:
//!   Giriş: kapanış önceki 20-bar zirvesini aşarsa Long [0.7];
//!          20-bar dibi aşarsa Short
//!   Filtre: 55-bar kanal — fiyat üstündeyse trend sürüyor (piramit
//!           koşulu yeni 20-bar kırılımı), altındaysa long Exit
//!   Birim: %1 equity / ATR14 → ATR-bazlı boyut
//! GEREKLİ VERİ: >=56 mum + equity.
use crate::{closes, highs, lows, MarketInput, Signal, Strategy};
use math::indicators;

pub struct TurtleTrendFollowing;

impl Strategy for TurtleTrendFollowing {
    fn name(&self) -> &'static str {
        "Turtle"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let (h, l, c) = (highs(&input.candles), lows(&input.candles), closes(&input.candles));
        if c.len() < 56 {
            return Signal::Skip { reason: ">=56 mum gerekli (Donchian 20/55)".into() };
        }
        let price = c[c.len() - 1];
        let n = 20usize;

        // Donchian 20/55
        let don = indicators::donchian(&h, &l, n);
        let don55 = indicators::donchian(&h, &l, 55);
        let hh20 = don[don.len() - 2].0.unwrap(); // önceki barın kanalı (kırılım: kapanış öncesinin üstünde)
        let ll20 = don[don.len() - 2].1.unwrap();
        let hh55 = don55[don55.len() - 2].0.unwrap();
        let ll55 = don55[don55.len() - 2].1.unwrap();

        // ATR birim: N = ATR14
        let atr_n = indicators::atr(&h, &l, &c, 14).last().and_then(|x| *x).unwrap_or(0.0);
        if atr_n <= 0.0 {
            return Signal::Skip { reason: "ATR hesaplanamadı".into() };
        }
        let unit_usd = input.equity * 0.01 / atr_n * atr_n; // %1 equity risk / ATR$ -> birim
        let units = (unit_usd / atr_n).max(0.0);

        // 20 barlık kırılım (S1 giriş): kapanış önceki 20-bar zirve üstünde
        if price > hh20 {
            return Signal::Long {
                strength: 0.7,
                note: format!("20-bar kırılım: {price:.0} > {hh20:.0}; birim={units:.2} (ATR={atr_n:.0}), 55-bar üstü={} piramit onayı", price > hh55),
            };
        }
        if price < ll20 {
            return Signal::Short {
                strength: 0.7,
                note: format!("20-bar alt kırılım: {price:.0} < {ll20:.0}"),
            };
        }
        // 55 barlık trend filtresi: 55 üstü ama 20 kırılımı yok -> trend içinde tut
        if price > hh55 {
            return Signal::Hold { note: format!("55-bar üstü trend sürüyor ({price:.0} > {hh55:.0}) — piramit koşulu: yeni 20-bar kırılımı bekle") };
        }
        if price < ll55 {
            return Signal::Exit { note: format!("55-bar altı — long trend bitti ({price:.0} < {ll55:.0})") };
        }
        Signal::Hold { note: "kanal içinde".into() }
    }
}
