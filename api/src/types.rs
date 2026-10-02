//! Yanıt şemaları — info-endpoint dokümantasyonundaki wire formatları.
//!
//! Alan adları Hyperliquid'in kendi isimleriyle (camelCase) birebir tutulur:
//! PerpMeta (name, szDecimals, maxLeverage, isDelisted, marginMode),
//! Meta{universe}, AssetCtx (funding, openInterest, dayNtlVlm, premium,
//! oraclePx, markPx, midPx, impactPxs), Candle (t,T,s,i,o,c,h,l,v,n),
//! L2Book {coin, time, levels[0]=bids, levels[1]=asks}, FundingEntry.
//! max_px_decimals(): fiyat için izin verilen hane = 6 - szDecimals (perp).
#![allow(non_snake_case)]

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PerpMeta {
    pub name: String,
    pub szDecimals: u32,
    pub maxLeverage: u32,
    #[serde(default)]
    pub isDelisted: Option<bool>,
    #[serde(default)]
    pub onlyIsolated: Option<bool>,
    #[serde(default)]
    pub marginMode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Meta {
    pub universe: Vec<PerpMeta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetCtx {
    pub funding: String,
    pub openInterest: String,
    pub dayNtlVlm: String,
    #[serde(default)]
    pub premium: Option<String>,
    pub oraclePx: String,
    pub markPx: String,
    #[serde(default)]
    pub midPx: Option<String>,
    #[serde(default)]
    pub prevDayPx: Option<String>,
    #[serde(default)]
    pub impactPxs: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Candle {
    /// Açılış zamanı (unix ms)
    pub t: i64,
    /// Kapanış zamanı (unix ms)
    #[serde(rename = "T")]
    pub close_t: i64,
    pub s: String,
    pub i: String,
    pub o: String,
    pub c: String,
    pub h: String,
    pub l: String,
    pub v: String,
    pub n: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Level {
    pub px: String,
    pub sz: String,
    pub n: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct L2Book {
    pub coin: String,
    pub time: i64,
    /// levels[0] = bids, levels[1] = asks
    pub levels: Vec<Vec<Level>>,
}

impl L2Book {
    pub fn bids(&self) -> &[Level] {
        self.levels.first().map(|v| v.as_slice()).unwrap_or(&[])
    }
    pub fn asks(&self) -> &[Level] {
        self.levels.get(1).map(|v| v.as_slice()).unwrap_or(&[])
    }
    pub fn best_bid_px(&self) -> Option<f64> {
        self.bids().first().and_then(|l| l.px.parse().ok())
    }
    pub fn best_ask_px(&self) -> Option<f64> {
        self.asks().first().and_then(|l| l.px.parse().ok())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FundingEntry {
    pub coin: String,
    pub fundingRate: String,
    #[serde(default)]
    pub premium: Option<String>,
    pub time: i64,
}

impl PerpMeta {
    /// Fiyat için izin verilen maksimum ondalık hane (perp: 6 - szDecimals, max 5 significant figure)
    pub fn max_px_decimals(&self) -> u32 {
        6u32.saturating_sub(self.szDecimals)
    }
}
