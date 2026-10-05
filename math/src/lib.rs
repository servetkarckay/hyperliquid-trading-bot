//! MATH — saf, deterministik sayısal çekirdek.
//!
//! Kurallar: yalnız f64, harici bağımlılık yok (serde rand yok), aynı
//! girdi → aynı çıktı. Alt modüller: `indicators` (göstergeler + vol
//! modelleri), `linalg` (matris/OLS), `portfolio` (performans/Kelly),
//! `stats` (istatistik testleri). Referans doğrulama: Python karşılıkları
//! ile <1e-9 karşılaştırma testleri.
pub mod indicators;
pub mod linalg;
pub mod portfolio;
pub mod stats;
