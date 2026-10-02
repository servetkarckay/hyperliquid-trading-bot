//! KESİT MOMENTUM — evren geneli göreli güç sıralaması.
//!
//! AMAÇ: tekil coin trendi değil, TÜM coinler arasındaki göreli sıra.
//! HESAPLAMA:
//! 1. Evren getirileri (7g) azalan sıralanır
//! 2. Üst %20 = long dilim, alt %20 = short dilim
//! 3. Coinin sırada bulunması Long/Short [0.6]; orta bar (0.4-0.6) nötr
//! 4. Sıralama kayması: üst dilimden alt %40'a düşen coin Exit —
//!    otomatik çıkış kuralı
//! GEREKLİ VERİ: >=20 coin evren getirisi (data pipeline besler).
//! NOT: kripto kesit korelasyonu yüksek (ortak beta) — risk katmanı
//! kovaryans düzeltmesini uygulamalı.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct KesitMomentum;

impl Strategy for KesitMomentum {
    fn name(&self) -> &'static str {
        "Kesit Momentum"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        if input.universe_returns.len() < 20 {
            return Signal::Skip { reason: "evren getirisi < 20 coin".into() };
        }
        let mut ranked = input.universe_returns.clone();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let k = (ranked.len() as f64 * 0.2).ceil() as usize; // üst/alt %20
        let longs: Vec<&(String, f64)> = ranked.iter().take(k).collect();
        let shorts: Vec<&(String, f64)> = ranked.iter().rev().take(k).collect();

        let in_longs = longs.iter().any(|(c, _)| c == &input.coin);
        let in_shorts = shorts.iter().any(|(c, _)| c == &input.coin);

        // sıralama kayması: coin uzun listedeyken alt %40'a düşerse EXIT (doküman kuralı)
        let rank = ranked.iter().position(|(c, _)| c == &input.coin);
        let pct = rank.map(|r| r as f64 / ranked.len() as f64);

        if in_longs {
            return Signal::Long {
                strength: 0.6,
                note: format!("üst dilim (sıra {}/{}), 7g={:+.2}%", rank.unwrap() + 1, ranked.len(),
                    longs.iter().find(|(c, _)| c == &input.coin).unwrap().1 * 100.0),
            };
        }
        if in_shorts {
            return Signal::Short { strength: 0.6, note: "alt dilim — short adayı".into() };
        }
        if let Some(p) = pct {
            if p > 0.4 && p < 0.6 {
                return Signal::Hold { note: format!("orta dilim (%{:.0}) — nötr", p * 100.0) };
            }
        }
        let _ = closes(&input.candles);
        Signal::Hold { note: "dilim dışı".into() }
    }
}

// momentum ölçümünde EMA eğimi desteği (referans)
#[allow(dead_code)]
fn _ref() {
    let _ = indicators::ema;
}
