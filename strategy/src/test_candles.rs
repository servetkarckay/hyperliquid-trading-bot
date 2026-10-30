//! STRATEJİ TEST YARDIMCILARI — deterministik mum üretimi (yalnız test).
//!
//! Sabit tohumlu, zamansız seriler: `candle` (kapalı mum), `flat` (düz
//! seri), `linear` (rampa), `set/set_last` (bar değiştirme), `universe`
//! (kesit), `rows` (özel diziler), `zigzag` (pivot şematiği), `sine`
//! (döngü), `oscillator` (genlik rejimi), `ctx_state` (OI/funding) —
//! testler ENV/zamana/rastgeleliğe BAĞLI DEĞİLDİR; aynı girdi → aynı
//! sinyal.
#[cfg(test)]
pub mod helpers {
    use crate::Candle;

    /// Kapalı mum (test tabanı)
    pub fn candle(t: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> Candle {
        Candle {
            t,
            o,
            h,
            l,
            c,
            v,
            n: 1,
            closed: true,
        }
    }

    /// Sabit fiyatlı düz seri (volume dolu) — indikatör ısınması için
    pub fn flat(n: usize, px: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| candle(i as i64 * 3_600_000, px, px * 1.001, px * 0.999, px, 100.0))
            .collect()
    }

    /// Lineer trend: px = base + step*i; hacim sabit
    pub fn linear(n: usize, base: f64, step: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let px = base + step * i as f64;
                candle(i as i64 * 3_600_000, px, px * 1.001, px * 0.999, px, 100.0)
            })
            .collect()
    }

    /// Bar i'yi güncelle (son barı "kırılım" yapar)
    pub fn set_last(candles: &mut [Candle], c: f64, h: f64, l: f64, v: f64) {
        let n = candles.len();
        let o = candles[n - 1].o;
        let t = candles[n - 1].t;
        candles[n - 1] = candle(t, o, h, l, c, v);
    }

    /// Belirli bir endeks değerini değiştir (pivotsuz bölgeler için uçlar)
    pub fn set(candles: &mut [Candle], idx: usize, c: f64, h: f64, l: f64, v: f64) {
        let o = candles[idx].o;
        let t = candles[idx].t;
        candles[idx] = candle(t, o, h, l, c, v);
    }

    /// Getiri serisinden sabit universe_returns üret (kesit için)
    pub fn universe(n: usize) -> Vec<(String, f64)> {
        (0..n)
            .map(|i| (format!("C{i:02}"), i as f64 / 100.0))
            .collect()
    }

    /// Belirli indeksten başlayan (o,h,l,c,v) satırlarından mumlar üret —
    /// özel seriler (Wyckoff şematikleri, Turtle kırılım dizileri).
    pub fn rows(start_idx: i64, rows: &[(f64, f64, f64, f64, f64)]) -> Vec<Candle> {
        rows.iter()
            .enumerate()
            .map(|(k, (o, h, l, c, v))| {
                candle((start_idx + k as i64) * 3_600_000, *o, *h, *l, *c, *v)
            })
            .collect()
    }

    /// Pivot interpolasyonlu zigzag — Dow Theory ve Elliott Wave şematik
    /// testleri (aynı üretici; up_v/down_v ayak hacimleri).
    pub fn zigzag(pivots: &[(usize, f64)], n: usize, up_v: f64, down_v: f64) -> Vec<Candle> {
        let mut candles = Vec::new();
        for i in 0..n {
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
            let v = if p1 >= p0 { up_v } else { down_v };
            candles.push(candle(
                i as i64 * 3_600_000,
                px,
                px + 0.05,
                px - 0.05,
                px,
                v,
            ));
        }
        candles
    }

    /// Sinüs döngüsü — döngüsel/indikatör metrikleri (Classic Indicators).
    pub fn sine(n: usize, period: f64, amp: f64, v: f64) -> Vec<Candle> {
        use std::f64::consts::PI;
        (0..n)
            .map(|i| {
                let px = 100.0 + amp * (2.0 * PI * i as f64 / period).sin();
                candle(i as i64 * 3_600_000, px, px + 0.05, px - 0.05, px, v)
            })
            .collect()
    }

    /// Genlik-salınımlı seri: px = 100 ± amp(i) — volatilite rejimi
    /// geçişleri (GARCH sıkışma/bulge, Squeeze plato serileri).
    /// `wick`: gövde dışı sabit mesafe.
    pub fn oscillator<F: Fn(usize) -> f64>(n: usize, amp: F, v: f64, wick: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let px = 100.0 + amp(i) * (if i % 2 == 0 { 1.0 } else { -1.0 });
                candle(i as i64 * 3_600_000, px, px + wick, px - wick, px, v)
            })
            .collect()
    }

    /// ctx builder — OI/Funding strateji testleri (varsayılan sağlıklı alanlar).
    pub fn ctx_state(
        coin: &str,
        funding: f64,
        open_interest: f64,
        mark_px: f64,
    ) -> data::market_ctx::CtxState {
        data::market_ctx::CtxState {
            coin: coin.into(),
            funding,
            open_interest,
            mark_px,
            oracle_px: mark_px,
            premium: 0.0,
            day_ntl_vlm: 1_000_000.0,
            prev_day_px: mark_px,
            ts: 0,
        }
    }

    /// Determinizm garantisi: aynı parametre → birebir aynı seri.
    #[test]
    fn ureticiler_deterministik() {
        let a = flat(10, 100.0);
        let b = flat(10, 100.0);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(
                (x.t, x.o, x.h, x.l, x.c, x.v),
                (y.t, y.o, y.h, y.l, y.c, y.v)
            );
        }
        let z1 = zigzag(&[(0, 100.0), (40, 120.0)], 80, 250.0, 50.0);
        let z2 = zigzag(&[(0, 100.0), (40, 120.0)], 80, 250.0, 50.0);
        assert_eq!(z1.len(), z2.len());
        for (x, y) in z1.iter().zip(z2.iter()) {
            assert_eq!(
                (x.t, x.o, x.h, x.l, x.c, x.v),
                (y.t, y.o, y.h, y.l, y.c, y.v)
            );
        }
        let s1 = oscillator(20, |i| (i % 3) as f64, 100.0, 0.05);
        let s2 = oscillator(20, |i| (i % 3) as f64, 100.0, 0.05);
        assert_eq!(s1.len(), s2.len());
        for (x, y) in s1.iter().zip(s2.iter()) {
            assert_eq!(
                (x.t, x.o, x.h, x.l, x.c, x.v),
                (y.t, y.o, y.h, y.l, y.c, y.v)
            );
        }
    }

    /// zigzag: pivotlar tam bar değerlerinde çıkar.
    #[test]
    fn zigzag_pivot_degerleri() {
        let z = zigzag(&[(0, 100.0), (40, 120.0), (80, 100.0)], 80, 250.0, 50.0);
        assert_eq!(z[0].c, 100.0); // ilk pivot
        assert_eq!(z[40].c, 120.0); // orta pivot
        assert_eq!(z[79].c, 100.5); // interpolasyon: son bara yarım adım kala
    }
}
