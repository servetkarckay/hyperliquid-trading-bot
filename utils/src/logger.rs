//! LOGGER — tracing subscriber kurulumu.
//!
//! `init`: LOG_LEVEL > RUST_LOG > "info" önceliğiyle tek seferlik
//! (`Once`) subscriber kurar; .env'i kendisi yüklemeyi dener.
//! İkinci çağrı no-op — çift subscriber yasak.
use std::sync::Once;

static INIT: Once = Once::new();

/// .env'den LOG_LEVEL okuyup subscriber'ı kurar. Tek seferlik; ikinci çağrı no-op.
/// Öncelik: LOG_LEVEL (.env) > RUST_LOG > "info"
pub fn init() {
    INIT.call_once(|| {
 // config'den önce çağrılabilirse de .env'i dene
        if dotenvy::dotenv().is_err() {
            let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
        }
        let level = std::env::var("LOG_LEVEL").unwrap_or_default();
        let filter = if level.is_empty() {
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        } else {
            tracing_subscriber::EnvFilter::new(&level)
        };
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .compact()
            .init();
    });
}
