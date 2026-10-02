//! # math
//!
//! Strateji hesaplama kütüphanesi — saf Rust f64, deterministik, harici
//! bağımlılık yok. Her formül doc-comment'te tanımlıdır ve Python/numpy
//! referans implementasyonuyla sabit geçmiş verisi üzerinde karşılaştırılarak
//! sıfıra yakın sapma (<1e-9, pratikte ~1e-16) kanıtlanmıştır.
//! - indicators.rs : SMA/EMA/RSI/MACD/Bollinger/ATR/ADX/±DI/OBV/Stochastic/
//!                   Donchian/Fibonacci/VWAP/TWAP + realized/EWMA/Parkinson/
//!                   Garman-Klass/GARCH(1,1) volatilite modelleri
//! - portfolio.rs  : Sharpe/Sortino/Calmar/max drawdown/expectancy/Kelly +
//!                   kovaryans/korelasyon matrisleri + min-variance ağırlıklar
//! - linalg.rs     : matris işlemleri, OLS (t, R²), Gauss, Cholesky, ters
//! - stats.rs      : z-score, ADF birim kök testi, Hurst üssü, OU half-life
pub mod indicators;
pub mod linalg;
pub mod portfolio;
pub mod stats;
