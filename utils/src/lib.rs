//! UTILS — çapraz-kesit yardımcıları.
//!
//! `logger` — tracing subscriber kurulumu (tek seferlik).
//! `time` — NTP (SNTP v4) saat senkronu; `now_ms` NTP offset'li ms döner.
//! Bot'un zaman duyarlı yolları (timeout, mühür, candle bucket) bu
//! senkronizasyonu kullanır.

pub mod logger;
pub mod time;
