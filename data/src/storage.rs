//! STORAGE — kalıcılık soyutlaması (trait + gerçekleştirmeler).
//!
//! 1. `Storage` trait: trades / candles / ctx / funding kaydetme-okuma.
//! 2. `MemoryStorage`: test ve düşük-konfig fallback.
//!
//! Canlı bot state'i JSON dosyalarında tutulur (risk_state.json,
//! track_state.json, trade_exits.jsonl). Bu modül yalnız test/fallback
//! içindir.
use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;

use crate::aggregator::Candle;

const INSERT_RETRIES: u32 = 2; // ilkinden sonra tek retry — sonsuz retry yasak
const INSERT_RETRY_MS: u64 = 500;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TradeRecord {
    pub coin: String,
    pub px: f64,
    pub sz: f64,
    pub ts: i64,
    pub is_buy: bool,
}

#[async_trait]
pub trait Storage: Send {
    async fn save_trades(&mut self, trades: &[TradeRecord]) -> Result<usize>;
    async fn save_candles(
        &mut self,
        coin: &str,
        interval_secs: u64,
        candles: &[Candle],
    ) -> Result<usize>;
    async fn load_candles(
        &mut self,
        coin: &str,
        interval_secs: u64,
        limit: usize,
    ) -> Result<Vec<Candle>>;
    async fn trade_count(&self) -> Result<u64>;
    /// Piyasa bağlamı snapshot'ları (funding/OI/mark serileri) — desteklemeyen backend no-op
    async fn save_ctx(&mut self, _ctxs: &[crate::market_ctx::CtxState]) -> Result<usize> {
        Ok(0)
    }
}

/// Bellek içi depolama — test ve geri dönüş (fallback) modu
#[derive(Default)]
pub struct MemoryStorage {
    trades: Vec<TradeRecord>,
    candles: HashMap<(String, u64), Vec<Candle>>,
    ctx_count: u64,
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn save_trades(&mut self, trades: &[TradeRecord]) -> Result<usize> {
        self.trades.extend_from_slice(trades);
        Ok(trades.len())
    }

    async fn save_candles(
        &mut self,
        coin: &str,
        interval_secs: u64,
        candles: &[Candle],
    ) -> Result<usize> {
        let v = self
            .candles
            .entry((coin.to_string(), interval_secs))
            .or_default();
        for c in candles {
            if let Some(last) = v.last_mut() {
                if last.t == c.t {
                    *last = c.clone(); // aynı bucket -> güncelle
                    continue;
                }
            }
            v.push(c.clone());
        }
        Ok(candles.len())
    }

    async fn load_candles(
        &mut self,
        coin: &str,
        interval_secs: u64,
        limit: usize,
    ) -> Result<Vec<Candle>> {
        let v = self
            .candles
            .get(&(coin.to_string(), interval_secs))
            .cloned()
            .unwrap_or_default();
        Ok(v.into_iter().rev().take(limit).rev().collect())
    }

    async fn trade_count(&self) -> Result<u64> {
        Ok(self.trades.len() as u64)
    }

    async fn save_ctx(&mut self, ctxs: &[crate::market_ctx::CtxState]) -> Result<usize> {
        self.ctx_count += ctxs.len() as u64;
        Ok(ctxs.len())
    }
}
