//! # hyperliquid-api
//!
//! Hyperliquid veri erişim katmanı (REST + WebSocket + imzalı emir iletimi).
//! Bu katman PROJE TEKNİ BEYANI: her fonksiyon Hyperliquid resmi API'siyle
//! birebir çalışır, imza zinciri resmi Python SDK 0.24 ile bayt-bayt
//! doğrulanmıştır. Bot mantığı BURADA YOK — sadece veri erişimi.
//!
//! Dosyalar (resmi dokümantasyon sayfalarıyla birebir):
//! - signing.rs            -> api/signing.md + nonces-and-api-wallets.md
//!                            EIP-712 phantom agent imzası, msgpack action hash,
//!                            secp256k1, nonce (unix ms) yönetimi
//! - info_endpoint.rs      -> api/info-endpoint.md (+perpetuals,+spot)
//!                            Piyasa ve hesap sorguları: meta, metaAndAssetCtxs,
//!                            allMids, l2Book, candleSnapshot, fundingHistory,
//!                            clearinghouseState, userFees, openOrders, userFills
//! - exchange_endpoint.rs  -> api/exchange-endpoint.md (+tick-and-lot-size.md)
//!                            Emir yerleştirme/iptal/değiştirme, kaldıraç,
//!                            izole marjin, scheduleCancel + tick/lot yuvarlama
//! - websocket.rs          -> api/websocket.md (+subscriptions,+timeouts)
//!                            Canlı akışlar: trades, l2Book (anlık snapshot),
//!                            candle, allMids, activeAssetCtx, orderUpdates,
//!                            userEvents; 50sn ping + otomatik reconnect
//! - error_responses.rs    -> api/error-responses.md
//!                            Hata tipleri: HTTP, API mesajı, imza, WebSocket
//! - types.rs              -> info-endpoint yanıt şemaları (wire format, camelCase)
//! - config.rs             -> .env yükleme + doğrulama (anahtarlar, URL'ler)
//!
//! Kullanıcı sorgularında (clearinghouseState vb.) GERÇEK hesap (master)
//! adresi kullanılır; agent adresi boş sonuç döner (resmi kural).
pub mod error_responses;
pub mod config;
pub mod exchange_endpoint;
pub mod info_endpoint;
pub mod signing;
pub mod types;
pub mod websocket;

pub use config::Config;
pub use exchange_endpoint::{round_5sig, round_px_for, round_px_wire, round_sz_up, Exchange, OrderStatuses, Tif};
pub use info_endpoint::Info;
pub use signing::{now_ms, HyperliquidClient, L1Signature};
pub use websocket::WsClient;
