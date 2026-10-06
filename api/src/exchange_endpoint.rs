//! EMİR İLETİMİ — Hyperliquid exchange endpoint zarfları.
//!
//! Görevler:
//! 1. `Exchange::place_limit / place_ioc / place_trigger / batch_place /
//! batch_cancel`: tek ve toplu emir iletimi (cloid'li idempotency).
//! 2. `OrderStatuses`: yanıt durum ayrıştırma — `resting_oid` (dinledi),
//! `filled` (anında doldu), `error`.
//! 3. `WireOrder / BatchPlace / Tif`: wire şemaları.
//! 4. Yuvarlama yardımcıları: `round_5sig / round_px_wire / round_px_for /
//! round_sz_up` — borsanın 5-anlamlı-basamak fiyat ve szDecimals lot
//! kuralları.
use serde_json::{json, Value};

use crate::signing::{float_to_wire, HyperliquidClient, ExchangeOutcome};
use crate::config::Config;
use crate::error_responses::{ApiError, Result};

#[derive(Debug, Clone, Copy)]
pub enum Tif {
 /// post-only: anında eşleşirse iptal
    Alo,
 /// immediate-or-cancel
    Ioc,
 /// good-til-canceled
    Gtc,
}

impl Tif {
    fn wire(&self) -> &'static str {
        match self {
            Tif::Alo => "Alo",
            Tif::Ioc => "Ioc",
            Tif::Gtc => "Gtc",
        }
    }
}

#[derive(Clone)]
pub struct Exchange {
    pub c: HyperliquidClient,
}

/// Order/cancel yanıtından emir çıkarımı
pub struct OrderStatuses {
    pub raw: Value,
}

impl OrderStatuses {
    pub fn status(&self, i: usize) -> Option<&Value> {
        self.raw.pointer(&format!("/response/data/statuses/{i}"))
    }
    pub fn resting_oid(&self, i: usize) -> Option<i64> {
        self.status(i)?.get("resting")?.get("oid")?.as_i64()
    }
    pub fn filled(&self, i: usize) -> Option<(f64, f64, i64)> {
        let f = self.status(i)?.get("filled")?;
        Some((
            f.get("totalSz")?.as_str()?.parse().ok()?,
            f.get("avgPx")?.as_str()?.parse().ok()?,
            f.get("oid")?.as_i64()?,
        ))
    }
    pub fn error(&self, i: usize) -> Option<&str> {
        self.status(i)?.get("error").and_then(Value::as_str)
    }
}

/// 5 significant figure'a yuvarla (tick kurali: fiyat en fazla 5 anlamlı hane)
pub fn round_5sig(x: f64) -> f64 {
    if x == 0.0 {
        return 0.0;
    }
    let exp = x.abs().log10().floor() as i32 - 4;
    let factor = 10f64.powi(exp);
    (x / factor).round() * factor
}

/// Fiyat: 5 anlamli hane + perp MAX_DECIMALS(6) siniri.
/// 5 sig fig tek basina yetmiyor: 1e-8'lik fiyat 7 haneye yuvarlaniyor ve
/// borsa "Order has invalid price." donuyor.
pub fn round_px_wire(x: f64) -> f64 {
    let r = round_5sig(x);
    let c = (r * 1e6).round() / 1e6;
    if c > 0.0 { c } else { 0.0 }
}

/// Fiyatı coin'in TICK'Ine oturt (`max_decimals` = tick'in azami hane sayısı).
/// Testnet'te Not: (probe_px): ME (szDecimals=1) 0.073706'ı REDDEDİYOR,
/// 0.073710'u KABUL ediyor; MEME (szDecimals=0) 6 haneyi kabul ediyor.
/// 5 anlamlı hane sınırı da korunur.
pub fn round_px_for(x: f64, max_decimals: u32) -> f64 {
    if !(x > 0.0) || !x.is_finite() {
        return 0.0;
    }
 // SIRA ÖNEMLİ: önce 5 anlamlı hane, sonra tick ızgarası. Ters sırada
 // 84730.9 -> 84731.0 olur ve grid bozulur.
    let f = 10f64.powi((max_decimals as i32).clamp(0, 8));
    let snapped = (round_5sig(x) * f).round() / f;
    if snapped > 0.0 {
        snapped
    } else {
        0.0
    }
}

/// Boyutu szDecimals hanesine yukari yuvarla (min notional guvencesi icin)
pub fn round_sz_up(x: f64, sz_decimals: u32) -> f64 {
    let d = 10f64.powi(sz_decimals as i32);
    (x * d).ceil() / d
}

/// Yürütülecek emir (batch formu — tek API çağrısında çoğaltılabilir)
#[derive(Debug, Clone)]
pub struct WireOrder {
    pub asset: u32,
    pub is_buy: bool,
    pub sz: f64,
    pub px: f64,
    pub tif: Tif,
    pub reduce_only: bool,
 /// İstemci tarafı idempotency anahtarı (16 haneli hex).
 ///
 /// `signing::post` 429/5xx'te isteği **yeniden gönderiyor**. 5xx'te
 /// istek borsada İŞLENMİŞ olabilir ama yanıt kaybolmuştur; retry aynı
 /// emri ikinci kez yollar ve bot pozisyonu ikiye katlar. Bu, 'in
 /// ("borsada emir var, bizde yok") çözdüğü senaryonun bir ikizi — üstelik
 /// retry yüzünden BİLİNÇLİ olarak üretiliyor.
 ///
 /// Hyperliquid aynı `c` (cloid) ile ikinci emri **reddeder**
 /// ("Resting maker orders must have at least one of cloid or oid"), yani
 /// idempotency bedavaya geliyor. `None` ise alan hiç gönderilmez (eski
 /// davranış, geriye dönük uyum).
    pub cloid: Option<String>,
}

impl Exchange {
    pub fn new(cfg: Config) -> Result<Self> {
        Ok(Self { c: HyperliquidClient::new(cfg)? })
    }
}

/// `batch_place` sonucu. UNKNOWN ayrı bir değerdir — `Err` değil,
/// çünkü "emir borsada mı" sorusu hâlâ açıktır ve çağıran taraf
/// reconciliation yapmak ZORUNDADIR.
pub enum BatchPlace {
 /// Borsa yanıtı geldi (status'lar okunabilir).
    Known(OrderStatuses),
 /// 5xx/timeout: istek gönderildi, sonuç bilinmiyor.
    Unknown,
 /// İstek hiç gönderilemedi (imza hatası, 4xx, ağ hatası) — UNKNOWN DEĞİL.
    Error(ApiError),
}

impl Exchange {
 /// ÇOKLU EMİR TEK ÇAĞRI — UNKNOWN korunur (caller reconciliation yapar).
    pub async fn batch_place_full(&self, orders: &[WireOrder]) -> BatchPlace {
        if orders.is_empty() {
            return BatchPlace::Known(OrderStatuses { raw: json!({}) });
        }
        let wires: Vec<Value> = orders
            .iter()
            .map(|o| {
                let mut w = json!({
                    "a": o.asset, "b": o.is_buy,
                    "p": float_to_wire(round_px_wire(o.px)), "s": float_to_wire(o.sz),
                    "r": o.reduce_only, "t": { "limit": { "tif": o.tif.wire() } }
                });
                if let Some(c) = &o.cloid {
                    w["c"] = json!(c);
                }
                w
            })
            .collect();
        let action = json!({ "type": "order", "orders": wires, "grouping": "na" });
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(raw) => BatchPlace::Known(OrderStatuses { raw }),
            ExchangeOutcome::Unknown => BatchPlace::Unknown,
            ExchangeOutcome::Error(e) => BatchPlace::Error(e),
        }
    }

 /// ÇOKLU EMİR TEK ÇAĞRI (batching — rate limit dostu).
 /// UNKNOWN burada hata olarak yutulur; UNKNOWN'a ihtiyacı olan çağıranlar
 /// [`Self::batch_place_full`] kullanmalı.
    pub async fn batch_place(&self, orders: &[WireOrder]) -> Result<OrderStatuses> {
        match self.batch_place_full(orders).await {
            BatchPlace::Known(s) => Ok(s),
            BatchPlace::Unknown => Err(ApiError::Api("exchange unknown".into())),
            BatchPlace::Error(e) => Err(e),
        }
    }

 /// ÇOKLU İPTAL TEK ÇAĞRI
    pub async fn batch_cancel(&self, cancels: &[(u32, i64)]) -> Result<OrderStatuses> {
        if cancels.is_empty() {
            return Ok(OrderStatuses { raw: json!({}) });
        }
        let action = json!({
            "type": "cancel",
            "cancels": cancels.iter().map(|(a, o)| json!({ "a": a, "o": o })).collect::<Vec<_>>()
        });
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(raw) => Ok(OrderStatuses { raw }),
            ExchangeOutcome::Unknown => Err(ApiError::Api("exchange unknown".into())),
            ExchangeOutcome::Error(e) => Err(e),
        }
    }

/// Limit emir (SDK bulk_orders formati)
    pub async fn place_limit(
        &self,
        asset: u32,
        is_buy: bool,
        sz: f64,
        limit_px: f64,
        tif: Tif,
        reduce_only: bool,
    ) -> Result<OrderStatuses> {
        self.place_limit_cloid(asset, is_buy, sz, limit_px, tif, reduce_only, None)
            .await
    }

 /// `place_limit` + **açık `cloid`** (idempotency anahtarı).
 ///
 /// `cloid` emir nesnesinin İÇİNDE `{"c": "0x..."}` olarak gider (128 bit
 /// hex, `0x` + 32 hane — 16 hane API 422 döndürür).
    pub async fn place_limit_cloid(
        &self,
        asset: u32,
        is_buy: bool,
        sz: f64,
        limit_px: f64,
        tif: Tif,
        reduce_only: bool,
        cloid: Option<&str>,
    ) -> Result<OrderStatuses> {
        let mut order = json!({
            "a": asset,
            "b": is_buy,
            "p": float_to_wire(round_px_wire(limit_px)),
            "s": float_to_wire(sz),
            "r": reduce_only,
            "t": { "limit": { "tif": tif.wire() } }
        });
        if let Some(c) = cloid {
            order["c"] = json!(c);
        }
        let action = json!({ "type": "order", "orders": [order], "grouping": "na" });
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(raw) => Ok(OrderStatuses { raw }),
            ExchangeOutcome::Unknown => Err(ApiError::Api("exchange unknown".into())),
            ExchangeOutcome::Error(e) => Err(e),
        }
    }

 /// IOC piyasa benzeri emir: fiyata agresif limit + IOC
    pub async fn place_ioc(
        &self,
        asset: u32,
        is_buy: bool,
        sz: f64,
        limit_px: f64,
    ) -> Result<OrderStatuses> {
        self.place_limit(asset, is_buy, sz, limit_px, Tif::Ioc, false).await
    }

 /// Trigger (TP/SL) emri
    pub async fn place_trigger(
        &self,
        asset: u32,
        is_buy: bool,
        sz: f64,
        trigger_px: f64,
        is_market: bool,
        is_take_profit: bool,
        reduce_only: bool,
        cloid: Option<&str>,
    ) -> Result<OrderStatuses> {
        let mut order = json!({
            "a": asset,
            "b": is_buy,
            "p": float_to_wire(round_px_wire(if is_market { trigger_px * if is_buy { 1.05 } else { 0.95 } } else { trigger_px })),
            "s": float_to_wire(sz),
            "r": reduce_only,
            "t": { "trigger": { "isMarket": is_market, "triggerPx": float_to_wire(round_px_wire(trigger_px)), "tpsl": if is_take_profit { "tp" } else { "sl" } } }
        });
        if let Some(c) = cloid {
            order["c"] = json!(c);
        }
        let action = json!({ "type": "order", "orders": [order], "grouping": "na" });
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(raw) => Ok(OrderStatuses { raw }),
            ExchangeOutcome::Unknown => Err(ApiError::Api("exchange unknown".into())),
            ExchangeOutcome::Error(e) => Err(e),
        }
    }

 /// oid ile emir iptali (SDK bulk_cancel formati)
    pub async fn cancel(&self, asset: u32, oid: i64) -> Result<OrderStatuses> {
        let action = json!({ "type": "cancel", "cancels": [{ "a": asset, "o": oid }] });
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(raw) => Ok(OrderStatuses { raw }),
            ExchangeOutcome::Unknown => Err(ApiError::Api("exchange unknown".into())),
            ExchangeOutcome::Error(e) => Err(e),
        }
    }

 /// cloid ile iptal
    pub async fn cancel_by_cloid(&self, asset: u32, cloid: &str) -> Result<OrderStatuses> {
        let action =
            json!({ "type": "cancelByCloid", "cancels": [{ "asset": asset, "cloid": cloid }] });
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(raw) => Ok(OrderStatuses { raw }),
            ExchangeOutcome::Unknown => Err(ApiError::Api("exchange unknown".into())),
            ExchangeOutcome::Error(e) => Err(e),
        }
    }

 /// Emir duzenleme (batchModify)
    pub async fn modify(
        &self,
        asset: u32,
        oid: i64,
        is_buy: bool,
        sz: f64,
        limit_px: f64,
        tif: Tif,
    ) -> Result<OrderStatuses> {
        let order = json!({
            "a": asset,
            "b": is_buy,
            "p": float_to_wire(round_5sig(limit_px)),
            "s": float_to_wire(sz),
            "r": false,
            "t": { "limit": { "tif": tif.wire() } }
        });
        let action = json!({ "type": "batchModify", "modifies": [{ "oid": oid, "order": order }] });
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(raw) => Ok(OrderStatuses { raw }),
            ExchangeOutcome::Unknown => Err(ApiError::Api("exchange unknown".into())),
            ExchangeOutcome::Error(e) => Err(e),
        }
    }

 /// Kaldirac ayari (cross/isolated + leverage)
    pub async fn update_leverage(
        &self,
        asset: u32,
        is_cross: bool,
        leverage: u32,
    ) -> Result<Value> {
        let action = json!({
            "type": "updateLeverage",
            "asset": asset,
            "isCross": is_cross,
            "leverage": leverage
        });
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(v) => Ok(v),
            ExchangeOutcome::Unknown => Err(ApiError::Api("exchange unknown".into())),
            ExchangeOutcome::Error(e) => Err(e),
        }
    }

 /// Isolated margin ekleme/çekme (ntli = USD * 1e6)
    pub async fn update_isolated_margin(&self, asset: u32, usd: f64) -> Result<Value> {
        let ntli = (usd * 1e6).round() as i64;
        let action = json!({
            "type": "updateIsolatedMargin",
            "asset": asset,
            "isBuy": true,
            "ntli": ntli
        });
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(v) => Ok(v),
            ExchangeOutcome::Unknown => Err(ApiError::Api("exchange unknown".into())),
            ExchangeOutcome::Error(e) => Err(e),
        }
    }

 /// Tum emirleri belirli zamanda iptal et (None: mevcut plani temizle)
    pub async fn schedule_cancel(&self, time_ms: Option<i64>) -> Result<Value> {
        let mut action = json!({ "type": "scheduleCancel" });
        if let Some(t) = time_ms {
            action["time"] = json!(t);
        }
        match self.c.exchange_action(action).await {
            ExchangeOutcome::Success(v) => Ok(v),
            ExchangeOutcome::Unknown => Err(ApiError::Api("exchange unknown".into())),
            ExchangeOutcome::Error(e) => Err(e),
        }
    }

 /// Guvenli test emri boyutu: min $10 notional guvence altina alinir
    pub fn min_test_sz(&self, px: f64, sz_decimals: u32) -> f64 {
        round_sz_up(12.0 / px, sz_decimals)
    }
}
