//! # utils
//!
//! Bot yardımcıları (api'den bağımsız, tüm crate'ler kullanabilir):
//! - logger.rs : tracing ile yapılandırılmış loglama (LOG_LEVEL env)
//! - time.rs   : NTP saat senkronu (SNTP v4, median ofset) + düzeltilmiş
//!               unix ms + gecikme (latency) ölçümü (REST RTT, measure)

pub mod logger;
pub mod time;
