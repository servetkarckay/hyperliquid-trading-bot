//! Depolama soyutlaması: Redis (sıcak) + ClickHouse (kalıcı) + Memory (yedek).
//!
//! ROLLER:
//! - RedisCache  : SICAK ÖNBELLEK. Son mids haritası ve coin başına son ctx,
//!   TTL ile yazılır. Strateji döngüsü milisaniyeler içinde okur; API'ye
//!   sorgu gitmez, bayat veri otomatik düşer. REDIS_URL gerekli.
//! - ClickHouseStorage: KALICI ARŞİV + analitik. Tüm tick'ler (hl_trades),
//!   kapanmış mumlar (hl_candles), ctx serileri (hl_ctx) JSONEachRow ile
//!   toplu INSERT edilir; SQL ile backtest/geçmiş analizi. CLICKHOUSE_URL.
//! - MemoryStorage: sunucu yoksa yedek + test referansı.
//!
//! Storage trait: save_trades, save_candles, load_candles, save_ctx,
//! trade_count. Yeni backend (örn. TimescaleDB) trait'i implemente eder.
use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;

use crate::aggregator::Candle;

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
    async fn save_candles(&mut self, coin: &str, interval_secs: u64, candles: &[Candle]) -> Result<usize>;
    async fn load_candles(&mut self, coin: &str, interval_secs: u64, limit: usize) -> Result<Vec<Candle>>;
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

    async fn save_candles(&mut self, coin: &str, interval_secs: u64, candles: &[Candle]) -> Result<usize> {
        let v = self.candles.entry((coin.to_string(), interval_secs)).or_default();
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

    async fn load_candles(&mut self, coin: &str, interval_secs: u64, limit: usize) -> Result<Vec<Candle>> {
        let v = self.candles.get(&(coin.to_string(), interval_secs)).cloned().unwrap_or_default();
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

/// ClickHouse — HTTP arayüzü (8123), JSONEachRow insert.
/// `CLICKHOUSE_URL` örn. `http://localhost:8123`; tablolar otomatik oluşur.
pub struct ClickHouseStorage {
    url: String,
    http: reqwest::Client,
}

impl ClickHouseStorage {
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("CLICKHOUSE_URL").unwrap_or_default();
        if url.is_empty() {
            anyhow::bail!("CLICKHOUSE_URL tanımlı değil");
        }
        Ok(Self { url, http: reqwest::Client::new() })
    }

    pub async fn exec(&self, sql: &str) -> Result<String> {
        Ok(self.http.post(&self.url).body(sql.to_string()).send().await?.text().await?)
    }

    /// Tabloları oluştur (varsa no-op) — ClickHouse kullanımı için public.
    pub async fn ensure_tables(&self) -> Result<()> {
        self.exec("CREATE TABLE IF NOT EXISTS hl_trades (coin String, px Float64, sz Float64, ts Int64, is_buy Bool) ENGINE = MergeTree ORDER BY (coin, ts)").await?;
        self.exec("CREATE TABLE IF NOT EXISTS hl_candles (coin String, interval UInt64, t Int64, o Float64, h Float64, l Float64, c Float64, v Float64, n UInt64) ENGINE = ReplacingMergeTree ORDER BY (coin, interval, t)").await?;
        self.exec("CREATE TABLE IF NOT EXISTS hl_funding (coin String, ts Int64, fundingRate Float64, premium Float64) ENGINE = ReplacingMergeTree ORDER BY (coin, ts)").await?;
        self.exec("CREATE TABLE IF NOT EXISTS hl_ctx (coin String, ts Int64, funding Float64, openInterest Float64, markPx Float64, oraclePx Float64, premium Float64, dayNtlVlm Float64, prevDayPx Float64) ENGINE = ReplacingMergeTree ORDER BY (coin, ts)").await?;
        self.exec("CREATE TABLE IF NOT EXISTS hl_strategy_performance (strategy String, interval UInt64, sample String, ts Int64, trades UInt64, winRate Float64, profitFactor Float64, totalReturn Float64, maxDrawdown Float64, sharpe Float64, avgBars Float64) ENGINE = ReplacingMergeTree ORDER BY (strategy, interval, sample, ts)").await?;
        Ok(())
    }

    /// İmza veri ekleme (Gerçek zamandan veriler; public yardımcı)
    pub async fn exec_insert(&self, sql: &str) -> Result<()> {
        self.exec(sql).await.ok();
        Ok(())
    }

    /// canlı/backtest strateji performansı → hl_strategy_performance
    /// Eğitim sonrası: strateji başına birleşik IS/OOS satırı (tam alan).
    pub async fn save_performance_row(&self, p: &StrategyPerformanceRow) -> Result<usize> {
        let row = serde_json::json!({
            "strategy": p.strategy, "interval": p.interval, "sample": p.sample,
            "ts": hyperliquid_api::now_ms() as i64,
            "trades": p.trades, "winRate": p.win_rate, "profitFactor": p.profit_factor,
            "totalReturn": p.total_return, "maxDrawdown": p.max_drawdown,
            "sharpe": p.sharpe, "avgBars": p.avg_bars
        });
        self.exec_insert(&format!(
            "INSERT INTO hl_strategy_performance FORMAT JSONEachRow\n{}",
            row
        ))
        .await?;
        Ok(1)
    }

    pub async fn save_performance(
        &self,
        strat: &str,
        trades: u64,
        win_rate: f64,
        sharpe: f64,
        sample: &str,
    ) -> Result<usize> {
        let row = serde_json::json!({
            "strategy": strat, "interval": 0u64, "sample": sample,
            "ts": hyperliquid_api::now_ms() as i64,
            "trades": trades, "winRate": win_rate, "profitFactor": 0.0,
            "totalReturn": 0.0, "maxDrawdown": 0.0, "sharpe": sharpe, "avgBars": 0.0
        });
        self.exec_insert(&format!(
            "INSERT INTO hl_strategy_performance FORMAT JSONEachRow\n{}",
            row.to_string()
        ))
        .await?;
        Ok(1)
    }

    /// funding tablosuna kayıt (backtest Kelly katmanı)
    pub async fn save_funding(
        &self,
        coin: &str,
        hist: &[hyperliquid_api::types::FundingEntry],
    ) -> Result<usize> {
        if hist.is_empty() {
            return Ok(0);
        }
        let rows: Vec<String> = hist
            .iter()
            .map(|e| {
                serde_json::json!({
                    "coin": coin, "ts": e.time,
                    "fundingRate": e.fundingRate.parse::<f64>().unwrap_or(0.0),
                    "premium": e.premium.clone().and_then(|p| p.parse::<f64>().ok()).unwrap_or(0.0)
                }).to_string()
            })
            .collect();
        self.exec_insert(&format!("INSERT INTO hl_funding FORMAT JSONEachRow\n{}", rows.join("\n"))).await?;
        Ok(hist.len())
    }
}

#[async_trait]
impl Storage for ClickHouseStorage {
    async fn save_ctx(&mut self, ctxs: &[crate::market_ctx::CtxState]) -> Result<usize> {
        if ctxs.is_empty() {
            return Ok(0);
        }
        let rows: Vec<String> = ctxs
            .iter()
            .map(|c| {
                serde_json::json!({
                    "coin": c.coin, "ts": c.ts, "funding": c.funding,
                    "openInterest": c.open_interest, "markPx": c.mark_px,
                    "oraclePx": c.oracle_px, "premium": c.premium,
                    "dayNtlVlm": c.day_ntl_vlm, "prevDayPx": c.prev_day_px
                })
                .to_string()
            })
            .collect();
        self.exec_insert(&format!(
            "INSERT INTO hl_ctx FORMAT JSONEachRow\n{}",
            rows.join("\n")
        ))
        .await?;
        Ok(ctxs.len())
    }

    async fn save_trades(&mut self, trades: &[TradeRecord]) -> Result<usize> {
        if trades.is_empty() {
            return Ok(0);
        }
        let rows: Vec<String> = trades
            .iter()
            .map(|t| {
                serde_json::json!({
                    "coin": t.coin, "px": t.px, "sz": t.sz, "ts": t.ts, "is_buy": t.is_buy
                })
                .to_string()
            })
            .collect();
        let sql = format!(
            "INSERT INTO hl_trades FORMAT JSONEachRow\n{}",
            rows.join("\n")
        );
        self.exec(&sql).await?;
        Ok(trades.len())
    }

    async fn save_candles(&mut self, coin: &str, interval_secs: u64, candles: &[Candle]) -> Result<usize> {
        if candles.is_empty() {
            return Ok(0);
        }
        let rows: Vec<String> = candles
            .iter()
            .map(|c| {
                serde_json::json!({
                    "coin": coin, "interval": interval_secs, "t": c.t,
                    "o": c.o, "h": c.h, "l": c.l, "c": c.c, "v": c.v, "n": c.n
                })
                .to_string()
            })
            .collect();
        self.exec_insert(&format!(
            "INSERT INTO hl_candles FORMAT JSONEachRow\n{}",
            rows.join("\n")
        ))
        .await?;
        Ok(candles.len())
    }

    async fn load_candles(&mut self, coin: &str, interval_secs: u64, limit: usize) -> Result<Vec<Candle>> {
        let sql = format!(
            "SELECT t, o, h, l, c, v, n FROM hl_candles WHERE coin='{coin}' AND interval={interval_secs} ORDER BY t DESC LIMIT {limit} FORMAT JSONEachRow"
        );
        let body = self.exec(&sql).await?;
        let mut out = Vec::new();
        for line in body.lines() {
            if line.trim().is_empty() { continue; }
            let v: serde_json::Value = serde_json::from_str(line)?;
            out.push(Candle {
                t: v["t"].as_i64().unwrap_or(0),
                o: v["o"].as_f64().unwrap_or(0.0),
                h: v["h"].as_f64().unwrap_or(0.0),
                l: v["l"].as_f64().unwrap_or(0.0),
                c: v["c"].as_f64().unwrap_or(0.0),
                v: v["v"].as_f64().unwrap_or(0.0),
                n: v["n"].as_u64().unwrap_or(0),
                closed: true,
            });
        }
        out.reverse();
        Ok(out)
    }

    async fn trade_count(&self) -> Result<u64> {
        // TSV: düz sayı döner (JSONEachRow {"count()":...} objesi döndürür)
        let r = self.exec("SELECT count() FROM hl_trades FORMAT TSV").await?;
        Ok(r.trim().parse().unwrap_or(0))
    }
}

/// Redis — sıcak önbellek (son mids/ctx/candle). `REDIS_URL` ile aktifleşir.
/// Bağımlılık: redis crate (tokio). Sunucu yoksa tüm metotlar hata döner;
/// kullanım `REDIS_URL` varlığına göre opsiyoneldir.
pub struct RedisCache {
    conn: redis::aio::ConnectionManager,
}

impl RedisCache {
    /// REDIS_URL .env'de tanımlıysa bağlanır, yoksa None.
    pub async fn from_env() -> Result<Option<Self>> {
        let url = std::env::var("REDIS_URL").unwrap_or_default();
        if url.is_empty() {
            return Ok(None);
        }
        let client = redis::Client::open(url)
            ?;
        let conn = redis::aio::ConnectionManager::new(client)
            .await
            ?;
        Ok(Some(Self { conn }))
    }

    /// Son mids haritasını JSON olarak yaz (TTL saniye)
    pub async fn set_mids(&mut self, mids: &HashMap<String, f64>, ttl: u64) -> Result<()> {
        let payload = serde_json::to_string(mids)?;
        redis::cmd("SETEX")
            .arg("hl:mids")
            .arg(ttl)
            .arg(payload)
            .query_async::<()>(&mut self.conn)
            .await?;
        Ok(())
    }

    pub async fn get_mids(&mut self) -> Result<Option<HashMap<String, f64>>> {
        let v: Option<String> = redis::cmd("GET").arg("hl:mids").query_async(&mut self.conn).await?;
        Ok(v.and_then(|s| serde_json::from_str(&s).ok()))
    }

    /// Son ctx state'i coin başına yaz (TTL saniye)
    pub async fn set_ctx(&mut self, c: &crate::market_ctx::CtxState, ttl: u64) -> Result<()> {
        let payload = serde_json::to_string(c)?;
        redis::cmd("SETEX")
            .arg(format!("hl:ctx:{}", c.coin))
            .arg(ttl)
            .arg(payload)
            .query_async::<()>(&mut self.conn)
            .await?;
        Ok(())
    }

    pub async fn ping(&mut self) -> Result<()> {
        redis::cmd("PING").query_async::<()>(&mut self.conn).await?;
        Ok(())
    }
}

/// `hl_strategy_performance` INSERT satırı.
#[derive(Debug, Clone)]
pub struct StrategyPerformanceRow {
    pub strategy: String,
    pub interval: u64,
    pub sample: String,
    pub trades: u64,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub total_return: f64,
    pub max_drawdown: f64,
    pub sharpe: f64,
    pub avg_bars: f64,
}
