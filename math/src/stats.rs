//! STATS — istatistik testleri (saf).
//!
//! `z_score, zscore_last`; `adf_tstat / adf_is_stationary` (kritik
//! değerler sabit tablodan); `ou_half_life` (Ornstein-Uhlenbeck);
//! `hurst` (R/S göznümü). None-safe: yetersiz pencere → None.
use crate::linalg;

/// Z-score: (x - mean) / std (ddof=1)
pub fn z_score(x: f64, data: &[f64]) -> Option<f64> {
    let n = data.len();
    if n < 2 {
        return None;
    }
    let m = data.iter().sum::<f64>() / n as f64;
    let var = data.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n - 1) as f64;
    if var <= 0.0 {
        return None;
    }
    Some((x - m) / var.sqrt())
}

/// Serinin son değerinin pencereye göre z-score'u (mean reversion sinyali)
pub fn zscore_last(window: &[f64]) -> Option<f64> {
    let (last, rest) = window.split_last()?;
    z_score(*last, rest)
}

/// ADF test istatistiği (sabit terimli, lag=0):
/// Δy_t = α + β y_{t-1} + ε_t ; β'nın t-istatistiği.
/// Kritik değerler (MacKinnon 2010, sabit terimli): %1: -3.43, %5: -2.86, %10: -2.57
pub const ADF_CRITICAL_1PCT: f64 = -3.43;
pub const ADF_CRITICAL_5PCT: f64 = -2.86;
pub const ADF_CRITICAL_10PCT: f64 = -2.57;

pub fn adf_tstat(y: &[f64]) -> Option<f64> {
    if y.len() < 10 {
        return None;
    }
    // regresyon: Δy_t = α + β y_{t-1}
    let dy: Vec<f64> = y.windows(2).map(|w| w[1] - w[0]).collect();
    let lag: Vec<f64> = y[..y.len() - 1].to_vec();
    let x: Vec<Vec<f64>> = lag.iter().map(|v| vec![1.0, *v]).collect();
    let r = linalg::ols(&x, &dy)?;
    Some(r.t_stat[1])
}

/// ADF kararı: t < kritik (daha negatif) -> durağan (birim kök reddi)
pub fn adf_is_stationary(y: &[f64], significance_pct: u32) -> Option<bool> {
    let t = adf_tstat(y)?;
    let crit = match significance_pct {
        1 => ADF_CRITICAL_1PCT,
        5 => ADF_CRITICAL_5PCT,
        10 => ADF_CRITICAL_10PCT,
        _ => return None,
    };
    Some(t < crit)
}

/// OU half-life: Δs_t = α + β s_{t-1} regresyonundan
/// half_life = -ln(2) / β (β < 0 ise anlamlı)
pub fn ou_half_life(spread: &[f64]) -> Option<f64> {
    if spread.len() < 3 {
        return None;
    }
    let ds: Vec<f64> = spread.windows(2).map(|w| w[1] - w[0]).collect();
    let lag: Vec<f64> = spread[..spread.len() - 1].to_vec();
    let (_a, b) = crate::linalg::linreg(&ds, &lag)?;
    if b >= 0.0 {
        return None;
    }
    Some(-(2.0f64).ln() / b)
}

/// Hurst üssü (R/S analizi, log-log eğim):
/// H < 0.5 mean-reverting, H = 0.5 random walk, H > 0.5 trending
pub fn hurst(series: &[f64], min_window: usize) -> Option<f64> {
    let rets: Vec<f64> = series.windows(2).map(|w| w[1] / w[0] - 1.0).collect();
    let n = rets.len();
    if n < min_window * 2 {
        return None;
    }
    let mut sizes = Vec::new();
    let mut rs_values = Vec::new();
    let mut w = min_window;
    while w < n {
        let chunks = n / w;
        let mut rs_sum = 0.0;
        let mut count = 0;
        for c in 0..chunks {
            let seg = &rets[c * w..(c + 1) * w];
            let m = seg.iter().sum::<f64>() / w as f64;
            let mut cum = 0.0;
            let mut mn = f64::MAX;
            let mut mx = f64::MIN;
            let mut var = 0.0;
            for v in seg {
                cum += v - m;
                mn = mn.min(cum);
                mx = mx.max(cum);
                var += (v - m).powi(2);
            }
            let sd = (var / w as f64).sqrt();
            if sd > 0.0 && mx > mn {
                rs_sum += (mx - mn) / sd;
                count += 1;
            }
        }
        if count > 0 {
            sizes.push(w as f64);
            rs_values.push(rs_sum / count as f64);
        }
        w *= 2;
    }
    if sizes.len() < 2 {
        return None;
    }
    // log-log regresyon eğimi = H
    let lxs: Vec<f64> = sizes.iter().map(|s| s.ln()).collect();
    let lys: Vec<f64> = rs_values.iter().map(|r| r.ln()).collect();
    let (_, h) = crate::linalg::linreg(&lys, &lxs)?;
    Some(h)
}
