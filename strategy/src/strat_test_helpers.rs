//! Strateji test yardımcıları — DETERMİNİSTIK mum üretimi.
//! (test-only modül; lib.rs'ten açılır)
#[cfg(test)]
pub mod helpers {
    use crate::Candle;

    /// Kapalı mum (test tabanı)
    pub fn candle(t: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> Candle {
        Candle { t, o, h, l, c, v, n: 1, closed: true }
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
    pub fn set_last(candles: &mut Vec<Candle>, c: f64, h: f64, l: f64, v: f64) {
        let n = candles.len();
        let o = candles[n - 1].o;
        let t = candles[n - 1].t;
        candles[n - 1] = candle(t, o, h, l, c, v);
    }

    /// Belirli bir endeks değerini değiştir (pivotsuz bölgeler için uçlar)
    pub fn set(candles: &mut Vec<Candle>, idx: usize, c: f64, h: f64, l: f64, v: f64) {
        let o = candles[idx].o;
        let t = candles[idx].t;
        candles[idx] = candle(t, o, h, l, c, v);
    }

    /// Getiri serisinden sabit universe_returns üret (kesit için)
    pub fn universe(n: usize) -> Vec<(String, f64)> {
        (0..n).map(|i| (format!("C{i:02}"), i as f64 / 100.0)).collect()
    }
}
