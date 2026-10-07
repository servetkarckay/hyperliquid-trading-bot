//! KONFİG — .env yükleme ve doğrulama.
//!
//! Görevler:
//! 1. `dotenvy` ile .env'i süreç ENV'ine yükler (secret export gereksinimi yok).
//! 2. `Config::from_env`: NETWORK / API_URL / WS_URL / PRIVATE_KEY /
//!
//! API_WALLET_ADDRESS zorunlu; https/wss ön ekleri ve 32 bayt hex-anahtar
//! DOĞRULANIR — hatalı konfig süreci anında düşürür.
//! 3. `pk_hex`: imza anahtarını ham bayt olarak döner.
//!
//! Not: bot'un diğer ENV'leri (ENGINE_*, TRAIL_*, LEARN_*) burada DEĞİL,
//! kendi modüllerinde okunur.
use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub network: String,
    pub api_url: String,
    pub ws_url: String,
    /// API wallet (agent) private key, 0x'siz 32 byte hex
    pub private_key: String,
    pub agent_address: String,
}

impl Config {
    /// .env'den yukler; private key formatini dogrular.
    pub fn from_env() -> Result<Self> {
        // once calisma dizini, sonra workspace koku dene
        if dotenvy::dotenv().is_err() {
            let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../.env"));
        }

        let cfg = Self {
            network: std::env::var("NETWORK").context("NETWORK eksik")?,
            api_url: std::env::var("HYPERLIQUID_API_URL").context("HYPERLIQUID_API_URL eksik")?,
            ws_url: std::env::var("HYPERLIQUID_WS_URL").context("HYPERLIQUID_WS_URL eksik")?,
            private_key: std::env::var("HYPERLIQUID_PRIVATE_KEY")
                .context("HYPERLIQUID_PRIVATE_KEY eksik")?,
            agent_address: std::env::var("API_WALLET_ADDRESS").unwrap_or_default(),
        };

        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let pk = self.private_key.trim().trim_start_matches("0x");
        if hex::decode(pk)
            .map_err(|e| anyhow::anyhow!("private key hex degil: {e}"))?
            .len()
            != 32
        {
            bail!("private key 32 byte olmali");
        }
        if !self.api_url.starts_with("https://") {
            bail!("api_url https olmali: {}", self.api_url);
        }
        if !self.ws_url.starts_with("wss://") {
            bail!("ws_url wss olmali: {}", self.ws_url);
        }
        Ok(())
    }

    /// 0x'siz, temiz 32 byte hex private key
    pub fn pk_hex(&self) -> String {
        self.private_key
            .trim()
            .trim_start_matches("0x")
            .to_ascii_lowercase()
    }
}
