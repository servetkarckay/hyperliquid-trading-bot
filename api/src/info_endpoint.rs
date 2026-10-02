//! REST info sorguları — resmi api/info-endpoint.md (+perpetuals,+spot).
//!
//! Stratejilerin ihtiyaç duyduğu tüm okuma fonksiyonları:
//! - meta / meta_and_asset_ctxs : evren (szDecimals, maxLeverage, isDelisted)
//!   + bağlam (funding, openInterest, dayNtlVlm, premium, mark/oracle/midPx)
//! - all_mids                   : tüm coinlerin mid fiyatları (+spot @index)
//! - l2_book                    : emir defteri snapshot (levels=[bids,asks])
//! - candle_snapshot            : OHLCV (max 5000; endTime GERÇEK ms şart)
//! - funding_history            : tarihsel funding oranları
//! - clearinghouse_state        : hesap durumu (GERÇEK master adres gerekli)
//! - user_fees / open_orders / user_fills / order_status : hesap sorguları
//! - predicted_fundings, spot_meta : ek kaynaklar
//! - name_to_asset              : coin adı -> perp asset ID (universe index)
//!
//! Tüm fonksiyonlar POST /info kullanır; paginasyon 500 kayıt/istek.
use std::collections::HashMap;

use serde_json::{json, Value};

use crate::signing::HyperliquidClient;
use crate::config::Config;
use crate::error_responses::Result;
use crate::types::{Candle, FundingEntry, L2Book, Meta, PerpMeta};

pub struct Info {
    pub c: HyperliquidClient,
}

impl Info {
    pub fn new(cfg: Config) -> Result<Self> {
        Ok(Self { c: HyperliquidClient::new(cfg)? })
    }

    /// Perp evreni + margin tabloları (szDecimals, maxLeverage, isDelisted)
    pub async fn meta(&self) -> Result<Meta> {
        let v = self.c.info(json!({ "type": "meta", "dex": "" })).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// Tek çağrıda evren + asset ctx (OI, funding, premium, dayNtlVlm, markPx...)
    pub async fn meta_and_asset_ctxs(&self) -> Result<(Meta, Vec<crate::types::AssetCtx>)> {
        let v = self
            .c
            .info(json!({ "type": "metaAndAssetCtxs", "dex": "" }))
            .await?;
        // 2026 formatı: [meta, ctxs] — ctx index = universe sırası
        let arr = v.as_array().filter(|a| a.len() == 2).cloned().ok_or_else(|| {
            crate::error_responses::ApiError::Api("metaAndAssetCtxs: [meta, ctxs] bekleniyordu".into())
        })?;
        let meta: Meta = serde_json::from_value(arr[0].clone())?;
        let ctxs: Vec<crate::types::AssetCtx> = serde_json::from_value(arr[1].clone())?;
        Ok((meta, ctxs))
    }

    /// Tüm coin'lerin mid fiyatları ({"BTC": "...", "#<oid>": ...})
    pub async fn all_mids(&self) -> Result<HashMap<String, String>> {
        let v = self.c.info(json!({ "type": "allMids", "dex": "" })).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// Emir defteri snapshot'u (tarafta 20 seviye)
    pub async fn l2_book(&self, coin: &str) -> Result<L2Book> {
        let v = self.c.info(json!({ "type": "l2Book", "coin": coin })).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// OHLCV mumları (max 5000; endTime GERÇEK timestamp olmalı — 0 => HTTP 500)
    pub async fn candle_snapshot(
        &self,
        coin: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Candle>> {
        let v = self.c.info(json!({
            "type": "candleSnapshot",
            "req": { "coin": coin, "interval": interval, "startTime": start_ms, "endTime": end_ms }
        })).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// Tarihsel funding oranları (coin, fundingRate, premium, time)
    pub async fn funding_history(
        &self,
        coin: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingEntry>> {
        let v = self.c.info(json!({
            "type": "fundingHistory", "coin": coin, "startTime": start_ms, "endTime": end_ms
        })).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// Hesap durumu (pozisyonlar, margin, nonce) — user: GERÇEK hesap adresi (agent değil!)
    pub async fn clearinghouse_state(&self, user: &str) -> Result<Value> {
        self.c.info(json!({ "type": "clearinghouseState", "user": user.to_ascii_lowercase() }))
            .await
    }

    /// Kullanıcı fee'leri (maker/taker) — user: gerçek hesap adresi
    pub async fn user_fees(&self, user: &str) -> Result<Value> {
        self.c.info(json!({ "type": "userFees", "user": user.to_ascii_lowercase() })).await
    }

    /// Açık emirler — user: gerçek hesap adresi
    pub async fn open_orders(&self, user: &str) -> Result<Value> {
        self.c.info(json!({ "type": "openOrders", "user": user.to_ascii_lowercase() })).await
    }

    /// İşlem geçmişi — user: gerçek hesap adresi
    pub async fn user_fills(&self, user: &str) -> Result<Value> {
        self.c.info(json!({ "type": "userFills", "user": user.to_ascii_lowercase() })).await
    }

    /// Emir durumu (oid veya cloid) — user: gerçek hesap adresi
    pub async fn order_status(&self, user: &str, oid: i64) -> Result<Value> {
        self.c.info(json!({ "type": "orderStatus", "user": user.to_ascii_lowercase(), "oid": oid }))
            .await
    }

    /// Tahmini funding oranları
    pub async fn predicted_fundings(&self) -> Result<Value> {
        self.c.info(json!({ "type": "predictedFundings" })).await
    }

    /// Spot meta (spot evreni + token'lar)
    pub async fn spot_meta(&self) -> Result<Value> {
        self.c.info(json!({ "type": "spotMeta" })).await
    }

    /// Coin adı -> perp asset ID (universe index). Meta'dan çözülür.
    pub async fn name_to_asset(&self, coin: &str) -> Result<(u32, PerpMeta)> {
        let meta = self.meta().await?;
        meta.universe
            .iter()
            .position(|m| m.name == coin)
            .map(|i| (i as u32, meta.universe[i].clone()))
            .ok_or_else(|| crate::error_responses::ApiError::Api(format!("coin evrende yok: {coin}")))
    }
}
