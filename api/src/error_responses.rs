//! HATA TİPLERİ — crate'in tek hata taksonomisi.
//!
//! `ApiError`: Http / Api (borsa red metni) / Json / Sign / Hex / Ws / Other.
//! Kural: "emin olunamayan" her hata (borsa UNKNOWN, bilinmeyen metin)
//! **kritik** sayılır (`track::classify` buradan beslenir) — sessiz
//! yutma yok.
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("API hatası: {0}")]
    Api(String),
    #[error("JSON hatası: {0}")]
    Json(#[from] serde_json::Error),
    #[error("İmza hatası: {0}")]
    Sign(String),
    #[error("Hex hatası: {0}")]
    Hex(#[from] hex::FromHexError),
    // BOYUT KURALI (clippy result_large_err): tungstenite::Error HTTP
    // yanıtını (headers) taşıyabilir — varyant enum'u 128+ byte'a çıkarır.
    // Box ile taşınır: semantik/değişmezler korunur (sadece dolgu küçülür).
    #[error("WebSocket hatası: {0}")]
    Ws(Box<tokio_tungstenite::tungstenite::Error>),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError::Http {
            status: 0,
            body: e.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

impl From<tokio_tungstenite::tungstenite::Error> for ApiError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        ApiError::Ws(Box::new(e))
    }
}
