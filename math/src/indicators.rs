//! INDICATORS — klasik göstergeler + volatilite modelleri (saf).
//!
//! Trend/ momentum: `sma, ema, rsi, macd, adx, obv, stochastic`;
//! bantlar/kanallar: `bollinger, donchian, fib_levels`; fiyat: `atr,
//! rolling_vwap, twap`; volatilite: `realized_vol, ewma_vol, parkinson,
//! garman_klass, garch11_variance / garch11_fit` (deterministik fit),
//! `annualization_factor`. Dönüşler `Option` taşır — yetersiz pencere
//! None; stratejiler None'ı Skip'e çevirmek ZORUNDADIR (sahte yön yasağı).
pub fn sma(data: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; data.len()];
    if n == 0 {
        return out;
    }
    let mut sum = 0.0;
    for (i, v) in data.iter().enumerate() {
        sum += v;
        if i >= n {
            sum -= data[i - n];
        }
        if i + 1 >= n {
            out[i] = Some(sum / n as f64);
        }
    }
    out
}

/// EMA alpha = 2/(n+1), seed = data[0] (adjust=False)
pub fn ema(data: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; data.len()];
    if data.is_empty() || n == 0 {
        return out;
    }
    let alpha = 2.0 / (n as f64 + 1.0);
    out[0] = Some(data[0]);
    for i in 1..data.len() {
        let prev = out[i - 1].unwrap();
        out[i] = Some(alpha * data[i] + (1.0 - alpha) * prev);
    }
    out
}

/// Wilder EMA: alpha = 1/n, seed = data[0] (recursive)
fn wilder_ema_from(data: &[f64], n: usize, seed: f64) -> Vec<f64> {
    let alpha = 1.0 / n as f64;
    let mut out = Vec::with_capacity(data.len());
    let mut prev = seed;
    for &v in data {
        prev = alpha * v + (1.0 - alpha) * prev;
        out.push(prev);
    }
    out
}

/// RSI (Wilder, seed = ilk n değişimin basit ortalaması)
pub fn rsi(closes: &[f64], n: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; closes.len()];
    if closes.len() <= n || n == 0 {
        return out;
    }
    let mut gains = Vec::with_capacity(closes.len() - 1);
    let mut losses = Vec::with_capacity(closes.len() - 1);
    for w in closes.windows(2) {
        let d = w[1] - w[0];
        gains.push(d.max(0.0));
        losses.push((-d).max(0.0));
    }
    // seed: ilk n değişimin ortalaması (index n-1'e denk gelir)
    let avg_gain: f64 = gains[..n].iter().sum::<f64>() / n as f64;
    let avg_loss: f64 = losses[..n].iter().sum::<f64>() / n as f64;
    out[n] = Some(rsi_from(avg_gain, avg_loss));
    let g = wilder_ema_from(&gains[n..], n, avg_gain);
    let l = wilder_ema_from(&losses[n..], n, avg_loss);
    for i in n + 1..closes.len() {
        out[i] = Some(rsi_from(g[i - n - 1], l[i - n - 1]));
    }
    out
}

fn rsi_from(avg_gain: f64, avg_loss: f64) -> f64 {
    if avg_loss == 0.0 {
        return 100.0;
    }
    let rs = avg_gain / avg_loss;
    100.0 - 100.0 / (1.0 + rs)
}

/// MACD: ema_fast - ema_slow; signal = ema_n9(macd); hist = macd - signal
/// (macd None bölgeleri signal hesabına katılmaz: kırpılmış seriyle ilerler)
pub fn macd(
    closes: &[f64],
    fast: usize,
    slow: usize,
    signal: usize,
) -> (Vec<Option<f64>>, Vec<Option<f64>>, Vec<Option<f64>>) {
    let ef = ema(closes, fast);
    let es = ema(closes, slow);
    let macd_line: Vec<Option<f64>> = ef
        .iter()
        .zip(&es)
        .map(|(f, s)| match (f, s) {
            (Some(f), Some(s)) => Some(f - s),
            _ => None,
        })
        .collect();
    let first = macd_line.iter().position(|v| v.is_some()).unwrap_or(macd_line.len());
    let compact: Vec<f64> = macd_line[first..].iter().map(|v| v.unwrap()).collect();
    let sig_c = ema(&compact, signal);
    let mut sig = vec![None; macd_line.len()];
    for (i, v) in sig_c.iter().enumerate() {
        sig[first + i] = *v;
    }
    let hist: Vec<Option<f64>> = macd_line
        .iter()
        .zip(&sig)
        .map(|(m, s)| match (m, s) {
            (Some(m), Some(s)) => Some(m - s),
            _ => None,
        })
        .collect();
    (macd_line, sig, hist)
}

/// Bollinger: (orta=SMA n, üst, alt) = sma ± k * standart sapma (popülasyon, ddof=0)
pub fn bollinger(data: &[f64], n: usize, k: f64) -> Vec<(Option<f64>, Option<f64>, Option<f64>)> {
    let means = sma(data, n);
    let mut out = Vec::with_capacity(data.len());
    for i in 0..data.len() {
        match means[i] {
            Some(m) if i + 1 >= n => {
                let w = &data[i + 1 - n..=i];
                let var = w.iter().map(|v| (v - m).powi(2)).sum::<f64>() / n as f64;
                let sd = var.sqrt();
                out.push((Some(m), Some(m + k * sd), Some(m - k * sd)));
            }
            _ => out.push((None, None, None)),
        }
    }
    out
}

/// True Range: max(H-L, |H-Cprev|, |L-Cprev|); ilk bar: H-L
pub fn true_range(high: &[f64], low: &[f64], close: &[f64]) -> Vec<f64> {
    let mut tr = Vec::with_capacity(high.len());
    for i in 0..high.len() {
        if i == 0 {
            tr.push(high[0] - low[0]);
        } else {
            tr.push(
                (high[i] - low[i])
                    .max((high[i] - close[i - 1]).abs())
                    .max((low[i] - close[i - 1]).abs()),
            );
        }
    }
    tr
}

/// ATR (Wilder): seed = ilk n TR ortalaması
pub fn atr(high: &[f64], low: &[f64], close: &[f64], n: usize) -> Vec<Option<f64>> {
    let tr = true_range(high, low, close);
    let mut out = vec![None; high.len()];
    if high.len() < n + 1 || n == 0 {
        return out;
    }
    let seed: f64 = tr[1..=n].iter().sum::<f64>() / n as f64;
    out[n] = Some(seed);
    let rest = wilder_ema_from(&tr[n + 1..], n, seed);
    for (i, v) in rest.iter().enumerate() {
        out[n + 1 + i] = Some(*v);
    }
    out
}

/// ADX sistemi: (+DI, -DI, ADX) — Wilder. Dönüş: (plus_di, minus_di, adx)
pub fn adx(
    high: &[f64],
    low: &[f64],
    close: &[f64],
    n: usize,
) -> (Vec<Option<f64>>, Vec<Option<f64>>, Vec<Option<f64>>) {
    let len = high.len();
    let mut pdi = vec![None; len];
    let mut mdi = vec![None; len];
    let mut adx_out = vec![None; len];
    if len < 2 * n + 1 || n == 0 {
        return (pdi, mdi, adx_out);
    }
    // +DM / -DM / TR
    let mut plus_dm = vec![0.0; len - 1];
    let mut minus_dm = vec![0.0; len - 1];
    for i in 1..len {
        let up = high[i] - high[i - 1];
        let down = low[i - 1] - low[i];
        plus_dm[i - 1] = if up > down && up > 0.0 { up } else { 0.0 };
        minus_dm[i - 1] = if down > up && down > 0.0 { down } else { 0.0 };
    }
    let tr = true_range(high, low, close);
    let tr1 = &tr[1..];
    // seed: ilk n ortalaması
    let str1: f64 = tr1[..n].iter().sum::<f64>() / n as f64;
    let spdm: f64 = plus_dm[..n].iter().sum::<f64>() / n as f64;
    let smdm: f64 = minus_dm[..n].iter().sum::<f64>() / n as f64;
    pdi[n] = Some(100.0 * spdm / str1);
    mdi[n] = Some(100.0 * smdm / str1);
    let p_rest = wilder_ema_from(&plus_dm[n..], n, spdm);
    let m_rest = wilder_ema_from(&minus_dm[n..], n, smdm);
    let tr_rest = wilder_ema_from(&tr1[n..], n, str1);
    // DX serisi
    let mut dx: Vec<f64> = Vec::new();
    for i in 0..p_rest.len() {
        let p = 100.0 * p_rest[i] / tr_rest[i];
        let m = 100.0 * m_rest[i] / tr_rest[i];
        pdi[n + i] = Some(p);
        mdi[n + i] = Some(m);
        if p + m > 0.0 {
            dx.push(100.0 * (p - m).abs() / (p + m));
        } else {
            dx.push(0.0);
        }
    }
    if dx.len() >= n {
        let seed: f64 = dx[..n].iter().sum::<f64>() / n as f64;
        adx_out[2 * n] = Some(seed);
        let rest = wilder_ema_from(&dx[n..], n, seed);
        for (i, v) in rest.iter().enumerate() {
            adx_out[2 * n + 1 + i] = Some(*v);
        }
    }
    (pdi, mdi, adx_out)
}

/// OBV: kapanış artarsa +v, azalırsa -v, eşitse önceki
pub fn obv(close: &[f64], volume: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(close.len());
    let mut acc = 0.0;
    for i in 0..close.len() {
        if i > 0 {
            if close[i] > close[i - 1] {
                acc += volume[i];
            } else if close[i] < close[i - 1] {
                acc -= volume[i];
            }
        }
        out.push(acc);
    }
    out
}

/// Stochastic: %K = 100*(C - LL_n)/(HH_n - LL_n); %D = %K'nın SMA d'si
pub fn stochastic(
    high: &[f64],
    low: &[f64],
    close: &[f64],
    n: usize,
    d: usize,
) -> (Vec<Option<f64>>, Vec<Option<f64>>) {
    let len = close.len();
    let mut k = vec![None; len];
    for i in n - 1..len {
        if i + 1 >= n {
            let hh = high[i + 1 - n..=i].iter().cloned().fold(f64::MIN, f64::max);
            let ll = low[i + 1 - n..=i].iter().cloned().fold(f64::MAX, f64::min);
            if hh != ll {
                k[i] = Some(100.0 * (close[i] - ll) / (hh - ll));
            } else {
                k[i] = Some(50.0);
            }
        }
    }
    let kc: Vec<f64> = k.iter().filter_map(|v| *v).collect();
    let dc = sma(&kc, d);
    let mut d_out = vec![None; len];
    let first_k = k.iter().position(|v| v.is_some()).unwrap_or(len);
    for (i, v) in dc.into_iter().enumerate() {
        d_out[first_k + i] = v;
    }
    (k, d_out)
}

/// Donchian kanalları: (üst = HH n, alt = LL n, orta) — güncel bar dahil
pub fn donchian(high: &[f64], low: &[f64], n: usize) -> Vec<(Option<f64>, Option<f64>, Option<f64>)> {
    let mut out = Vec::with_capacity(high.len());
    for i in 0..high.len() {
        if i + 1 >= n {
            let hh = high[i + 1 - n..=i].iter().cloned().fold(f64::MIN, f64::max);
            let ll = low[i + 1 - n..=i].iter().cloned().fold(f64::MAX, f64::min);
            out.push((Some(hh), Some(ll), Some((hh + ll) / 2.0)));
        } else {
            out.push((None, None, None));
        }
    }
    out
}

/// Fibonacci düzeltme seviyeleri: swing düşük-yüksek aralığından 0.0..1.0
pub fn fib_levels(low: f64, high: f64) -> Vec<(f64, f64)> {
    let d = high - low;
    [0.0, 0.236, 0.382, 0.5, 0.618, 0.786, 1.0]
        .iter()
        .map(|r| (*r, high - d * r))
        .collect()
}

/// Rolling VWAP: son n barın (tipik fiyat × hacim) ortalaması
pub fn rolling_vwap(high: &[f64], low: &[f64], close: &[f64], volume: &[f64], n: usize) -> Vec<Option<f64>> {
    let tp: Vec<f64> = (0..close.len()).map(|i| (high[i] + low[i] + close[i]) / 3.0).collect();
    let mut out = vec![None; close.len()];
    let mut pv = 0.0;
    let mut v = 0.0;
    for i in 0..close.len() {
        pv += tp[i] * volume[i];
        v += volume[i];
        if i >= n {
            pv -= tp[i - n] * volume[i - n];
            v -= volume[i - n];
        }
        if i + 1 >= n && v > 0.0 {
            out[i] = Some(pv / v);
        }
    }
    out
}

/// TWAP: son n kapanışın eşit ağırlıklı ortalaması (zaman ağırlıklı)
pub fn twap(close: &[f64], n: usize) -> Vec<Option<f64>> {
    sma(close, n)
}

// ---- Volatilite modelleri ----

/// Log getiriler
pub fn log_returns(closes: &[f64]) -> Vec<f64> {
    closes.windows(2).map(|w| (w[1] / w[0]).ln()).collect()
}

/// Realized vol: pencere içi std (ddof=1) × sqrt(yıllıklaştırma çarpanı)
/// annualization: 1h için 24*365, 1d için 365, per-bar için 1
pub fn realized_vol(closes: &[f64], n: usize, annualization: f64) -> Vec<Option<f64>> {
    let r = log_returns(closes);
    let mut out = vec![None; closes.len()];
    for i in n..=r.len() {
        let w = &r[i - n..i];
        let m = w.iter().sum::<f64>() / n as f64;
        let var = w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1) as f64;
        out[i] = Some(var.sqrt() * annualization.sqrt());
    }
    out
}

/// EWMA vol (RiskMetrics): σ²_t = λσ²_{t-1} + (1-λ)r²_{t-1}, seed = ilk r²
pub fn ewma_vol(closes: &[f64], lambda: f64, annualization: f64) -> Vec<Option<f64>> {
    let r = log_returns(closes);
    let mut out = vec![None; closes.len()];
    if r.is_empty() {
        return out;
    }
    let mut var = r[0] * r[0];
    out[1] = Some(var.sqrt() * annualization.sqrt());
    for i in 1..r.len() {
        var = lambda * var + (1.0 - lambda) * r[i] * r[i];
        out[i + 1] = Some(var.sqrt() * annualization.sqrt());
    }
    out
}

/// Parkinson vol: sqrt(1/(4 ln 2) * mean(ln(H/L)²)) * sqrt(ann)
pub fn parkinson(high: &[f64], low: &[f64], n: usize, annualization: f64) -> Vec<Option<f64>> {
    let mut out = vec![None; high.len()];
    let c = 1.0 / (4.0 * std::f64::consts::LN_2);
    for i in 0..high.len() {
        if i + 1 >= n {
            let w = &high[i + 1 - n..=i];
            let wl = &low[i + 1 - n..=i];
            let m: f64 = w.iter().zip(wl).map(|(h, l)| (h / l).ln().powi(2)).sum::<f64>() / n as f64;
            out[i] = Some((c * m * annualization).sqrt());
        }
    }
    out
}

/// Garman-Klass vol: sqrt(mean(0.5 ln(H/L)² - (2ln2-1) ln(C/O)²)) * sqrt(ann)
pub fn garman_klass(
    high: &[f64],
    low: &[f64],
    open: &[f64],
    close: &[f64],
    n: usize,
    annualization: f64,
) -> Vec<Option<f64>> {
    let mut out = vec![None; high.len()];
    let c2 = 2.0 * std::f64::consts::LN_2 - 1.0;
    for i in 0..high.len() {
        if i + 1 >= n {
            let mut s = 0.0;
            for j in i + 1 - n..=i {
                let hl = (high[j] / low[j]).ln();
                let co = (close[j] / open[j]).ln();
                s += 0.5 * hl * hl - c2 * co * co;
            }
            out[i] = Some((s / n as f64 * annualization).sqrt());
        }
    }
    out
}

/// GARCH(1,1): σ²_t = ω + α ε²_{t-1} + β σ²_{t-1}
/// Döner: koşullu varyans serisi (r ile hizalı; seed = örneklem varyansı)
pub fn garch11_variance(returns: &[f64], omega: f64, alpha: f64, beta: f64) -> Vec<f64> {
    let n = returns.len();
    if n == 0 {
        return vec![];
    }
    let var0 = returns.iter().map(|r| r * r).sum::<f64>() / n as f64;
    let mut out = Vec::with_capacity(n);
    let mut prev_var = var0;
    out.push(var0);
    for i in 1..n {
        let v = omega + alpha * returns[i - 1] * returns[i - 1] + beta * prev_var;
        out.push(v);
        prev_var = v;
    }
    out
}

/// GARCH(1,1) fit — varyans hedeflemeli koordinat inişi (deterministik):
/// ω = var*(1-α-β); log-likelihood maksimize edilir (N(0,σ²) varsayımı).
pub fn garch11_fit(returns: &[f64], steps: usize) -> (f64, f64, f64, f64) {
    let var0 = returns.iter().map(|r| r * r).sum::<f64>() / returns.len() as f64;
    let mut best = (0.05f64, 0.85f64);
    let mut best_ll = f64::NEG_INFINITY;
    let grid: Vec<f64> = (1..=steps).map(|i| i as f64 / steps as f64 * 0.9).collect();
    for &alpha in &grid {
        for &beta in &grid {
            if alpha + beta >= 0.999 {
                continue;
            }
            let omega = var0 * (1.0 - alpha - beta);
            let vars = garch11_variance(returns, omega, alpha, beta);
            let ll: f64 = returns
                .iter()
                .zip(&vars)
                .map(|(r, v)| -0.5 * ((2.0 * std::f64::consts::PI) + v.ln() + r * r / v))
                .sum();
            if ll > best_ll {
                best_ll = ll;
                best = (alpha, beta);
            }
        }
    }
    let (alpha, beta) = best;
    let omega = var0 * (1.0 - alpha - beta);
    (omega, alpha, beta, best_ll)
}

/// Yıllıklaştırma çarpanı: bar başına saniye -> yıl (365 gün, kripto 7/24)
pub fn annualization_factor(bar_seconds: u64) -> f64 {
    let bars_per_year = 365.0 * 24.0 * 3600.0 / bar_seconds as f64;
    bars_per_year
}
