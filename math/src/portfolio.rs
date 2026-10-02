//! Portföy matematiği ve performans metrikleri — strateji 11 (Kelly/risk)
//! ve 12 (portföy teorisi).
//!
//! simple_returns; Sharpe (excess mean/std ·√ann), Sortino (downside std),
//! max_drawdown (peak-trough/peak), Calmar (CAGR/MDD), expectancy,
//! Kelly binary (p-(1-p)/b) ve sürekli (μ/σ²).
//! covariance_matrix (örneklem, ddof=1), correlation_matrix,
//! min_variance_weights (w=Σ⁻¹1/1'Σ⁻¹1), portfolio_vol (√(w'Σw)).
use crate::linalg;

/// Basit getiriler
pub fn simple_returns(prices: &[f64]) -> Vec<f64> {
    prices.windows(2).map(|w| w[1] / w[0] - 1.0).collect()
}

/// Sharpe oranı: mean(r - rf) / std(r, ddof=1) * sqrt(annualization)
/// rf per-bar risksiz getiri (0 = risksiz yok)
pub fn sharpe(returns: &[f64], rf_per_bar: f64, annualization: f64) -> Option<f64> {
    let n = returns.len();
    if n < 2 {
        return None;
    }
    let excess: Vec<f64> = returns.iter().map(|r| r - rf_per_bar).collect();
    let m = excess.iter().sum::<f64>() / n as f64;
    let var = excess.iter().map(|r| (r - m).powi(2)).sum::<f64>() / (n - 1) as f64;
    if var <= 0.0 {
        return None;
    }
    Some(m / var.sqrt() * annualization.sqrt())
}

/// Sortino: mean(r - rf) / downside std (sadece r < rf karelerinin ortalaması)
pub fn sortino(returns: &[f64], rf_per_bar: f64, annualization: f64) -> Option<f64> {
    let n = returns.len();
    if n < 2 {
        return None;
    }
    let excess: Vec<f64> = returns.iter().map(|r| r - rf_per_bar).collect();
    let m = excess.iter().sum::<f64>() / n as f64;
    let dd: f64 = excess
        .iter()
        .filter(|r| **r < 0.0)
        .map(|r| r.powi(2))
        .sum::<f64>()
        / n as f64;
    if dd <= 0.0 {
        return None;
    }
    Some(m / dd.sqrt() * annualization.sqrt())
}

/// Equity eğrisinden maksimum düşüş (oran, 0..1): (peak - trough)/peak
pub fn max_drawdown(equity: &[f64]) -> Option<f64> {
    let mut peak = f64::MIN;
    let mut mdd = 0.0;
    for v in equity {
        if *v > peak {
            peak = *v;
        }
        if peak > 0.0 {
            let dd = (peak - v) / peak;
            if dd > mdd {
                mdd = dd;
            }
        }
    }
    Some(mdd)
}

/// Calmar: CAGR / max drawdown
pub fn calmar(equity: &[f64], bars_per_year: f64) -> Option<f64> {
    if equity.len() < 2 || equity[0] <= 0.0 {
        return None;
    }
    let years = equity.len() as f64 / bars_per_year;
    let cagr = (equity[equity.len() - 1] / equity[0]).powf(1.0 / years) - 1.0;
    let mdd = max_drawdown(equity)?;
    if mdd <= 0.0 {
        return None;
    }
    Some(cagr / mdd)
}

/// Beklenti (işlem başına): p*b - (1-p) — b: ortalama kazanç/ortalama kayıp oranı
pub fn expectancy(wins: usize, total: usize, avg_win: f64, avg_loss: f64) -> Option<f64> {
    if total == 0 || avg_loss <= 0.0 {
        return None;
    }
    let p = wins as f64 / total as f64;
    Some(p * avg_win - (1.0 - p) * avg_loss)
}

/// Kelly kriteri (binary): f* = p - (1-p)/b, b = win/loss oranı
pub fn kelly_binary(win_prob: f64, win_loss_ratio: f64) -> Option<f64> {
    if win_loss_ratio <= 0.0 || !(0.0..=1.0).contains(&win_prob) {
        return None;
    }
    Some(win_prob - (1.0 - win_prob) / win_loss_ratio)
}

/// Kelly (sürekli): f* = μ / σ²
pub fn kelly_continuous(mean_return: f64, variance: f64) -> Option<f64> {
    if variance <= 0.0 {
        return None;
    }
    Some(mean_return / variance)
}

/// Örneklem kovaryans matrisi (ddof=1): rows = varlık serileri
pub fn covariance_matrix(series: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let k = series.len();
    let n = series.first().map(|s| s.len()).unwrap_or(0);
    let means: Vec<f64> = series.iter().map(|s| s.iter().sum::<f64>() / n as f64).collect();
    let mut cov = vec![vec![0.0; k]; k];
    for i in 0..k {
        for j in i..k {
            let mut s = 0.0;
            for t in 0..n {
                s += (series[i][t] - means[i]) * (series[j][t] - means[j]);
            }
            let v = s / (n - 1) as f64;
            cov[i][j] = v;
            cov[j][i] = v;
        }
    }
    cov
}

/// Korelasyon matrisi (kovaryanstan normalize)
pub fn correlation_matrix(series: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let cov = covariance_matrix(series);
    let k = cov.len();
    let sd: Vec<f64> = (0..k).map(|i| cov[i][i].sqrt()).collect();
    let mut corr = vec![vec![0.0; k]; k];
    for i in 0..k {
        for j in 0..k {
            corr[i][j] = if sd[i] > 0.0 && sd[j] > 0.0 { cov[i][j] / (sd[i] * sd[j]) } else { 0.0 };
        }
    }
    corr
}

/// Min-variance portföy ağırlıkları: w = Σ⁻¹1 / (1'Σ⁻¹1)
/// (kısa pozisyon yasak değil; kısıtlı çözüm için sonraki sürüm)
pub fn min_variance_weights(cov: &[Vec<f64>]) -> Option<Vec<f64>> {
    let k = cov.len();
    let ones = vec![1.0; k];
    let inv = linalg::inverse(cov)?;
    let w = linalg::mat_vec(&inv, &ones);
    let denom: f64 = ones.iter().sum::<f64>();
    if denom.abs() < 1e-14 {
        return None;
    }
    let sum: f64 = w.iter().sum();
    if sum.abs() < 1e-14 {
        return None;
    }
    Some(w.iter().map(|x| x / sum).collect())
}

/// Portföy volatilitesi: sqrt(w' Σ w)
pub fn portfolio_vol(weights: &[f64], cov: &[Vec<f64>]) -> f64 {
    let k = weights.len();
    let mut tmp = vec![0.0; k];
    for i in 0..k {
        for j in 0..k {
            tmp[i] += cov[i][j] * weights[j];
        }
    }
    let var: f64 = weights.iter().zip(&tmp).map(|(w, t)| w * t).sum();
    var.max(0.0).sqrt()
}
