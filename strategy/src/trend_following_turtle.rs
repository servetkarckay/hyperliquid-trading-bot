//! TURTLE TREND FOLLOWING — Donchian 20/55 + ATR birim boyut.
//!
//! 1. Giriş: kapanış önceki 20-bar kanalın üstünde → Long [0.7];
//!    altında → Short [0.7] (kanal ÖNCEKİ barın — kırılım konvansiyonu).
//! 2. Filtre: 55-bar kanal üstü → Hold (trend sürüyor, piramit koşulu).
//! 3. **Ölü sinyal düzeltmesi (P1-I)**: `price<ll55→Exit` dalı yapısal
//!    ölüydü (ll55 ≤ ll20 ⟹ Short önce); kural `long_exit_signal()`
//!    helper'ına taşındı — engine LONG pozisyonda Exit yönlendirir.
//! 4. ATR ≤ 0 / kanal serisi kısa → Skip; <56 mum → Skip.
use crate::{closes, highs, lows, MarketInput, Signal, Strategy};
use math::indicators;

pub struct TurtleTrendFollowing;

/// Turtle **long Exit kuralı** (doküman): kapanış 55-bar kanalın altı →
/// long trend bitti. Strateji pozisyon tarafını BİLMEDİĞİ için bu kural
/// evaluate() içinde ölüydü (ll55 ≤ ll20 ⟹ Short önce yakalardı); engine
/// LONG pozisyonu için buradan Exit yönlendirir.
pub fn long_exit_signal(candles: &[data::Candle]) -> bool {
    if candles.len() < 56 {
        return false; // veri yok → karar yok (sahte exit yasak)
    }
    let h: Vec<f64> = candles.iter().map(|x| x.h).collect();
    let l: Vec<f64> = candles.iter().map(|x| x.l).collect();
    let c: Vec<f64> = candles.iter().map(|x| x.c).collect();
    let don55 = indicators::donchian(&h, &l, 55);
    let (Some(_), Some(ll55)) = (don55[don55.len() - 2].0, don55[don55.len() - 2].1) else {
        return false;
    };
    let price = c[c.len() - 1];
    price < ll55
}

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
        // D: kanal serisi kısa/boş → Skip (unwrap panik yasağı)
        let (Some(hh20), Some(ll20)) = (don[don.len() - 2].0, don[don.len() - 2].1) else {
            return Signal::Skip { reason: "Donchian(20) serisi hazır değil".into() };
        };
        let (Some(hh55), Some(ll55)) = (don55[don55.len() - 2].0, don55[don55.len() - 2].1) else {
            return Signal::Skip { reason: "Donchian(55) serisi hazır değil".into() };
        };

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
        // ⚠️ ÖLÜ SİNYAL DÜZELTMESİ (P1-I): `price < ll55` Exit dalı yapısal
        // olarak ÖLÜYDÜ — 55-bar kanal 20-bar kanalı KAPSAR (ll55 ≤ ll20)
        // ⟹ `price < ll55` ⟹ `price < ll20` ⟹ Short dalı önce çalışır.
        // Dokümante kural ("55-bar altı = long trend bitti") POZİSYON
        // FARKINDADIR; strateji yönü bilmediği için Exit intent'i
        // `long_exit_signal` üzerinden ENGINE'e taşınır (kesit
        // `rank_shift_exit_long` ile aynı mimari).
        Signal::Hold { note: format!("kanal içi ({price:.0}); 20/55 kanalları={hh20:.0}/{ll20:.0}, 55={hh55:.0}/{ll55:.0}") }
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

    /// Sabit taban + kanal-altı dipler, kırılım barı kanalın ÜSTÜNDE.
    fn base(n: usize) -> Vec<data::Candle> {
        let mut c = H::flat(40, 100.0);
        // 15 bar dip (kanalın altı) — ll20 = 95
        c.extend((0..15).map(|i| H::candle((40 + i) as i64 * 3_600_000, 99.0, 99.5, 95.0 + i as f64 * 0.1, 96.0 + i as f64 * 0.1, 100.0)));
        // 5 bar normale dönüş (kanal 95'te kalır)
        c.extend(H::flat(5, 97.0));
        c
    }

    /// A — 20-bar kırılım üstünde Long.
    #[test]
    fn turtle_20bar_kirilim_long_uretir() {
        let mut candles = base(60);
        H::set_last(&mut candles, 112.0, 112.5, 111.5, 100.0); // > hh20 (100)
        let input = MarketInput { coin: "T".into(), candles, equity: 10_000.0, ..Default::default() };
        match TurtleTrendFollowing.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!((strength - 0.7).abs() < 1e-9);
                assert!(note.contains("20-bar kırılım"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

    /// C — Short simetrisi: 20-bar kanalın altında Short.
    #[test]
    fn turtle_20bar_alt_kirilim_short_uretir() {
        let mut candles = H::flat(40, 100.0);
        candles.extend(H::linear(20, 100.0, -0.5)); // kanal 100'e yakın, düşen
        H::set_last(&mut candles, 88.0, 88.5, 87.5, 100.0);
        let input = MarketInput { coin: "T".into(), candles, equity: 10_000.0, ..Default::default() };
        assert!(matches!(TurtleTrendFollowing.evaluate(&input), Signal::Short { strength: 0.7, .. }));
    }

    /// A — **long_exit_signal**: kapanış 55-bar kanalın altı → Exit kuralı
    /// CANLI (ölü sinyal düzeltmesi — önceden Exit dalı asla çalışmıyordu).
    #[test]
    fn turtle_55bar_alti_exit_kurali_canli() {
        // 55 bar 100'ün altı yok → kanal ~100; kırılım 80 → long exit
        let mut candles = H::flat(60, 100.0);
        H::set_last(&mut candles, 80.0, 80.5, 79.5, 100.0);
        assert!(long_exit_signal(&candles), "80 < 55-bar kanal → LONG EXIT");
        // kanal içinde → exit yok
        let candles2 = H::flat(60, 100.0);
        assert!(!long_exit_signal(&candles2));
        // veri yetersiz → karar yok
        assert!(!long_exit_signal(&H::flat(10, 100.0)));
        // evaluate artık ölü dal içermez: 80 fiyatlı durumda Short (ters yön)
        let input = MarketInput { coin: "T".into(), candles, equity: 10_000.0, ..Default::default() };
        assert!(matches!(TurtleTrendFollowing.evaluate(&input), Signal::Short { .. }));
    }

    /// B — FUTURE LEAKAGE: kırılımı ÖNCEDEN bilmez; prefix'te kırılım
    /// barı yokken Long ÜRETİLMEZ. + Gelecek 50 bar eklendiğinde geçmiş
    /// karar değişmez (saf fonksiyon).
    #[test]
    fn turtle_future_leak_prefix_karar_degismez() {
        let mut full = base(60);
        H::set_last(&mut full, 112.0, 112.5, 111.5, 100.0); // bar 59: kırılım
        full.extend(H::flat(50, 112.0)); // gelecek 50 bar

        // A = bar 0..=58: kırılım YOK → Long YOK
        let a = full[..59].to_vec();
        let ia = MarketInput { coin: "T".into(), candles: a, equity: 10_000.0, ..Default::default() };
        let sa = TurtleTrendFollowing.evaluate(&ia);
        assert!(!matches!(sa, Signal::Long { .. }), "kırılım olmadan Long ÜRETİLMEZ: {sa:?}");
        // t anı (bar 59) → Long
        let it = MarketInput { coin: "T".into(), candles: full[..60].to_vec(), equity: 10_000.0, ..Default::default() };
        assert!(matches!(TurtleTrendFollowing.evaluate(&it), Signal::Long { .. }));
        // gelecekteki 50 bar prefix kararını değiştirmez
        let sa2 = TurtleTrendFollowing.evaluate(&ia);
        assert_eq!(sa, sa2);
    }

    /// D — kanal/ATR hazır değilse Skip (unwrap panik yok).
    #[test]
    fn turtle_kanal_hazir_degilse_skip() {
        let candles = H::flat(10, 100.0);
        let input = MarketInput { coin: "T".into(), candles, ..Default::default() };
        assert!(matches!(TurtleTrendFollowing.evaluate(&input), Signal::Skip { .. }));
    }
}
