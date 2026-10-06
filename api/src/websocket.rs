//! CANLI AKIŞ — Hyperliquid WebSocket istemcisi.
//!
//! `WsClient::run(subscriptions, tx)`: abonelikleri açar, her mesajı
//! `WsEvent { channel, data }` olarak pompalar.
//! 50 sn'de bir ping; sessiz bağlantı kopsun diye
//! 5 sn'de bir otomatik yeniden bağlanma (reconnect)
//! alıcı kapatılırsa görev sonlanır (leak yok)
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::config::Config;
use crate::error_responses::{ApiError, Result};

/// WS üzerinden gelen ham mesaj (channel + data)
#[derive(Debug, Clone)]
pub struct WsEvent {
    pub channel: String,
    pub data: Value,
}

pub struct WsClient {
    cfg: Config,
}

impl WsClient {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

 /// Bağlanır, abonelik açar, mesajları kanala pompalar; otomatik ping (50 sn) + reconnect (5 sn).
 /// Kalıcı görev olarak çalışır; drop edilen receiver temiz kapanir.
    pub async fn run(
        &self,
        subs: Vec<Value>,
        tx: mpsc::Sender<WsEvent>,
    ) -> Result<()> {
        let mut n_reconnect = 0u32;
        loop {
            match self.run_once(&subs, &tx).await {
                Ok(()) => return Ok(()), // receiver kapandı -> gorev bitti
                Err(e) => {
                    n_reconnect += 1;
                    tracing::warn!(
                        "WS hatası: {e}; 5 sn sonra yeniden bağlanıyor (reconnect #{n_reconnect})"
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn run_once(&self, subs: &Vec<Value>, tx: &mpsc::Sender<WsEvent>) -> Result<()> {
        let (ws, _) = tokio_tungstenite::connect_async(&self.cfg.ws_url)
            .await
            .map_err(|e| ApiError::Other(anyhow::anyhow!("WS bağlantı: {e}")))?;
        let (mut sink, mut stream) = ws.split();
        tracing::info!(
            "WS bağlandı: {} ({} aboneliği)",
            self.cfg.ws_url,
            subs.len()
        );

        for sub in subs {
            let msg = json!({ "method": "subscribe", "subscription": sub });
            sink.send(Message::text(msg.to_string())).await?;
        }

 // 50 sn'de bir ping (server 60 sn sessizlikte kapatır)
        let mut ping = tokio::time::interval(Duration::from_secs(50));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ping.tick() => {
                    if sink.send(Message::text(r#"{"method":"ping"}"#)).await.is_err() {
                        return Err(ApiError::Other(anyhow::anyhow!("ping gonderilemedi")));
                    }
                }
                msg = stream.next() => {
                    match msg {
                        None => return Err(ApiError::Other(anyhow::anyhow!("WS kapandı"))),
                        Some(m) => {
                            let data = m?;
                            if let Message::Text(txt) = data {
                                if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                    if v.get("channel").and_then(Value::as_str) == Some("pong") {
                                        continue;
                                    }
                                    let channel = v.get("channel").and_then(Value::as_str).unwrap_or("").to_string();
                                    let payload = v.get("data").cloned().unwrap_or(Value::Null);
                                    if tx.send(WsEvent { channel, data: payload }).await.is_err() {
                                        return Ok(()); // dinleyici kapandı
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
