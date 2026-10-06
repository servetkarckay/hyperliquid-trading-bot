//! LINALG — küçük yoğun matris işlemleri + OLS (saf, Option döner).
//!
//! `mat_mul, transpose, solve (Gauss eliminasyonu), inverse, cholesky,
//! ols (beta/t/R²), linreg`. Tekil/negatif yarı-tanımlı durumda None —
//! çağıran None'ı veri-yoksunluğu gibi ele alır (panik yok).
#[derive(Debug, Clone)]
pub struct OlsResult {
    pub beta: Vec<f64>,
    pub se: Vec<f64>,
    pub t_stat: Vec<f64>,
    pub r_squared: f64,
    pub n: usize,
    pub df: i64,
}

pub fn mat_mul(a: &[Vec<f64>], b: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = a.len();
    let m = b[0].len();
    let k = b.len();
    let mut c = vec![vec![0.0; m]; n];
    for i in 0..n {
        for p in 0..k {
            let aip = a[i][p];
            if aip == 0.0 {
                continue;
            }
            for j in 0..m {
                c[i][j] += aip * b[p][j];
            }
        }
    }
    c
}

pub fn transpose(a: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = a.len();
    let m = a[0].len();
    let mut t = vec![vec![0.0; n]; m];
    for i in 0..n {
        for j in 0..m {
            t[j][i] = a[i][j];
        }
    }
    t
}

pub fn mat_vec(a: &[Vec<f64>], x: &[f64]) -> Vec<f64> {
    a.iter()
        .map(|row| row.iter().zip(x).map(|(a, x)| a * x).sum())
        .collect()
}

/// Kısmi pivotlu Gauss eliminasyonu ile A x = b çözümü.
pub fn solve(a: &[Vec<f64>], b: &[f64]) -> Option<Vec<f64>> {
    let n = a.len();
    if n != b.len() {
        return None;
    }
    let mut m: Vec<Vec<f64>> = a
        .iter()
        .zip(b)
        .map(|(row, bi)| {
            let mut r = row.clone();
            r.push(*bi);
            r
        })
        .collect();

    for col in 0..n {
 // pivot seç
        let mut pivot = col;
        for r in col + 1..n {
            if m[r][col].abs() > m[pivot][col].abs() {
                pivot = r;
            }
        }
        if m[pivot][col].abs() < 1e-14 {
            return None; // tekil
        }
        m.swap(col, pivot);
        let inv = 1.0 / m[col][col];
        for r in col + 1..n {
            let f = m[r][col] * inv;
            if f != 0.0 {
                for c in col..=n {
                    m[r][c] -= f * m[col][c];
                }
            }
        }
    }
 // geri substitüsyon
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut s = m[i][n];
        for j in i + 1..n {
            s -= m[i][j] * x[j];
        }
        x[i] = s / m[i][i];
    }
    Some(x)
}

/// Gauss-Jordan ile ters matris (tekilse None).
pub fn inverse(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let mut m: Vec<Vec<f64>> = a
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut r = row.clone();
            r.extend((0..n).map(|j| if i == j { 1.0 } else { 0.0 }));
            r
        })
        .collect();

    for col in 0..n {
        let mut pivot = col;
        for r in col + 1..n {
            if m[r][col].abs() > m[pivot][col].abs() {
                pivot = r;
            }
        }
        if m[pivot][col].abs() < 1e-14 {
            return None;
        }
        m.swap(col, pivot);
        let inv = 1.0 / m[col][col];
        for c in 0..2 * n {
            m[col][c] *= inv;
        }
        for r in 0..n {
            if r != col && m[r][col] != 0.0 {
                let f = m[r][col];
                for c in 0..2 * n {
                    m[r][c] -= f * m[col][c];
                }
            }
        }
    }
    Some(m.into_iter().map(|r| r[n..].to_vec()).collect())
}

/// Cholesky: A (SPD) = L L^T. SPD değilse None.
pub fn cholesky(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let mut l = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in 0..=i {
            let mut s = a[i][j];
            for k in 0..j {
                s -= l[i][k] * l[j][k];
            }
            if i == j {
                if s <= 0.0 {
                    return None;
                }
                l[i][j] = s.sqrt();
            } else {
                l[i][j] = s / l[j][j];
            }
        }
    }
    Some(l)
}

/// OLS: y = X beta + e. X'e sabit terim dahil etmek çağıranın sorumluluğudur.
/// Döner: beta, standart hatalar, t-istatistikleri, R².
pub fn ols(x: &[Vec<f64>], y: &[f64]) -> Option<OlsResult> {
    let n = y.len();
    let k = x.first()?.len();
    if n <= k {
        return None;
    }
    let xt = transpose(x);
    let xtx = mat_mul(&xt, x);
    let xty: Vec<f64> = xt
        .iter()
        .map(|row| row.iter().zip(y).map(|(a, b)| a * b).sum())
        .collect();
    let beta = solve(&xtx, &xty)?;

 // fitted + residuals
    let mut rss = 0.0;
    let mut tss = 0.0;
    let ybar = y.iter().sum::<f64>() / n as f64;
    for (i, yi) in y.iter().enumerate() {
        let fit: f64 = x[i].iter().zip(&beta).map(|(a, b)| a * b).sum();
        rss += (yi - fit).powi(2);
        tss += (yi - ybar).powi(2);
    }
    let df = (n - k) as i64;
    let sigma2 = if df > 0 { rss / df as f64 } else { 0.0 };
    let xtx_inv = inverse(&xtx)?;
 // SE(beta_j) = sqrt(sigma2 * diag((X'X)^-1)_jj)
    let se: Vec<f64> = (0..k)
        .map(|j| (sigma2 * xtx_inv[j][j]).sqrt())
        .collect();
    let t_stat: Vec<f64> = beta.iter().zip(&se).map(|(b, s)| if *s > 0.0 { b / s } else { 0.0 }).collect();
    let r_squared = if tss > 0.0 { 1.0 - rss / tss } else { 0.0 };

    Some(OlsResult { beta, se, t_stat, r_squared, n, df })
}

/// Basit doğrusal regresyon: y = a + b x (tek değişkenli kapalı form)
pub fn linreg(y: &[f64], x: &[f64]) -> Option<(f64, f64)> {
    let n = x.len();
    if n != y.len() || n < 2 {
        return None;
    }
    let mx = x.iter().sum::<f64>() / n as f64;
    let my = y.iter().sum::<f64>() / n as f64;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for i in 0..n {
        sxy += (x[i] - mx) * (y[i] - my);
        sxx += (x[i] - mx).powi(2);
    }
    if sxx == 0.0 {
        return None;
    }
    let b = sxy / sxx;
    Some((my - b * mx, b))
}
