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
    #[error("WebSocket hatası: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError::Http { status: 0, body: e.to_string() }
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;
