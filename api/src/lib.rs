//! API — Hyperliquid veri ve emir erişim katmanı (façade).
//!
//! Tek görev: borsa I/O'sunu tipli, test edilebilir yüzeylere indirmek;
//! bot mantığı İÇERMEZ. Alt modüller:
//! `config` — .env yükleme/doğrulama (`Config`)
//! `signing` — EIP-712 phantom-agent imzası + HTTP istemci
//! `exchange_endpoint` — emir/iptal iletimi + fiyat/lot yuvarlama
//! `info_endpoint` — market/hesap okuma metotları
//! `error_responses` — `ApiError` hata taksonomisi
//! `types` — info endpoint wire şemaları
//! `websocket`— canlı akış (ping + otomatik yeniden bağlanma)
pub mod config;
pub mod error_responses;
pub mod exchange_endpoint;
pub mod info_endpoint;
pub mod signing;
pub mod types;
pub mod websocket;

pub use config::Config;
pub use exchange_endpoint::{
    perp_price, perp_price_verified, round_5sig, round_px_for, round_px_wire, round_sz_up,
    Exchange, OrderStatuses, Tif,
};
pub use info_endpoint::Info;
pub use signing::{now_ms, HyperliquidClient, L1Signature};
pub use websocket::WsClient;
