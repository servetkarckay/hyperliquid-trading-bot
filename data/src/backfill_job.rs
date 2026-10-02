//! TARİHİ VERİ POMPASI — backtest veri tabanı doldurma servisi.
//!
//! AMAÇ (araştırma kuralı): backtest için en az 6 ay (tercihen 1 yıl)
//! veri şart — rejim çeşitliliği (yatay/bull/bear/likidasyon kaskadları)
//! olmadan Kelly istatistiğine güvenilmez.
//!
//! KAYNAKLAR (paginasyon):
//! - candleSnapshot (5000 bar/istek) → hl_candles
//! - fundingHistory → hl_funding (Kelly strateji çarpan katmanı)
//!
//! RATE LIMIT: istekler arası uyku (varsayılan 250 ms).

use anyhow::Result;
use async_trait::async_trait;

use crate::storage::ClickHouseStorage;

pub const MAX_BARS_PER_REQUEST: usize = 5000;

#[derive(Debug, Clone)]
pub struct PumpConfig {
    pub coins: Vec<String>,
    pub interval_secs: u64,
    /// kaç gün geçmiş
    pub days: u64,
    /// istekler arası uyku ms
    pub request_pause_ms: u64,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            coins: vec![],
            interval_secs: 3600,
            days: 180,
            request_pause_ms: 250,
        }
    }
}

/// Candle mimari: interval sn → HL interval adı
pub fn interval_name(secs: u64) -> Result<&'static str> {
    Ok(match secs {
        60 => "1m", 300 => "5m", 900 => "15m", 1800 => "30m",
        3600 => "1h", 7200 => "2h", 14_400 => "4h", 86_400 => "1d",
        o => anyhow::bail!("bilinmeyen interval: {o}s"),
    })
}

/// Candle yazma soyutlaması (CH/parquet/Memory eklenebilir)
#[async_trait]
pub trait CandleSink: Send {
    async fn write(&mut self, coin: &str, interval_secs: u64, candles: &[hyperliquid_api::types::Candle]) -> Result<usize>;
}

#[async_trait]
impl CandleSink for ClickHouseStorage {
    async fn write(&mut self, coin: &str, interval_secs: u64, candles: &[hyperliquid_api::types::Candle]) -> Result<usize> {
        let rows: Vec<String> = candles
            .iter()
            .map(|c| {
                serde_json::json!({
                    "coin": coin, "interval": interval_secs, "t": c.t,
                    "o": c.o.parse::<f64>().ok(), "h": c.h.parse::<f64>().ok(),
                    "l": c.l.parse::<f64>().ok(), "c": c.c.parse::<f64>().ok(),
                    "v": c.v.parse::<f64>().ok(), "n": c.n.unwrap_or(0)
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
}

/// Bir coin'in geçmişini paginasyonla çeker ve sink'e yazar. Dönüş: bar sayısı.
pub async fn pump_candles(
    info: &hyperliquid_api::Info,
    coin: &str,
    interval_secs: u64,
    days: u64,
    request_pause_ms: u64,
    sink: &mut dyn CandleSink,
) -> Result<usize> {
    let end = hyperliquid_api::now_ms() as i64;
    let start = end - (days as i64 * 86_400_000);
    pump_candles_from(info, coin, interval_secs, start, end, request_pause_ms, sink).await
}

/// Verilen ms aralığını pompalar (resume: CH'daki son t'den itibaren)
pub async fn pump_candles_from(
    info: &hyperliquid_api::Info,
    coin: &str,
    interval_secs: u64,
    start: i64,
    end: i64,
    request_pause_ms: u64,
    sink: &mut dyn CandleSink,
) -> Result<usize> {
    let iv_name = interval_name(interval_secs)?;
    let step = (MAX_BARS_PER_REQUEST as i64) * (interval_secs as i64) * 1000;

    let mut total = 0usize;
    let mut cursor = start;
    while cursor < end {
        let req_end = (cursor + step).min(end);
        // 429 KIYAMET KORUMASI: hız sınırı → bekle + TEKRAR (ниче kayıp yok)
        match info.candle_snapshot(coin, iv_name, cursor, req_end).await {
            Ok(c) if !c.is_empty() => {
                sink.write(coin, interval_secs, &c).await?;
                total += c.len();
            }
            Err(e) => {
                tracing::warn!("{coin} {iv_name} [{cursor}..{req_end}]: {e} — 3 sn bekle, tekrar");
                tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
                match info.candle_snapshot(coin, iv_name, cursor, req_end).await {
                    Ok(c2) if !c2.is_empty() => {
                        sink.write(coin, interval_secs, &c2).await?;
                        total += c2.len();
                    }
                    Err(e2) => tracing::warn!("{coin} {iv_name} TEKRAR başarısız: {e2}"),
                    _ => {}
                }
            }
            _ => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(request_pause_ms)).await;
        cursor = req_end;
    }
    Ok(total)
}

/// Kompozit iş: coins × interval → ClickHouse + funding geçmişi
pub async fn run_pump(info: &hyperliquid_api::Info, ch: &ClickHouseStorage, cfg: PumpConfig) -> Result<String> {
    ch.ensure_tables().await?;
    let mut sink = ClickHouseStorage::from_env()?;
    let mut report = String::new();

    for coin in &cfg.coins {
        let n = pump_candles(info, coin, cfg.interval_secs, cfg.days, cfg.request_pause_ms, &mut sink).await;
        report += &match n {
            Ok(k) => format!("{coin}: {k} mum\n"),
            Err(e) => format!("{coin}: HATA {e}\n"),
        };
        match pump_funding(info, ch, coin, cfg.days).await {
            Ok(n) => report += &format!("{coin} funding: {n}\n"),
            Err(e) => report += &format!("{coin} funding: {e}\n"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(cfg.request_pause_ms)).await;
    }
    Ok(report)
}

/// funding geçmişi paginasyon (fundingHistory 500 pencere/istek)
pub async fn pump_funding(
    info: &hyperliquid_api::Info,
    ch: &ClickHouseStorage,
    coin: &str,
    days: u64,
) -> Result<usize> {
    let end = hyperliquid_api::now_ms() as i64;
    let start = end - (days as i64 * 86_400_000);
    let mut cursor = start;
    let mut total = 0;
    while cursor < end {
        let req_end = (cursor + 180 * 3_600_000).min(end);
        let hist = info.funding_history(coin, cursor, req_end).await?;
        total += ch.save_funding(coin, &hist).await?;
        cursor = req_end;
    }
    Ok(total)
}

impl ClickHouseStorage {
    /// coin+interval için depolanmış en son mum zamanı (resume noktası)
    pub async fn last_candle_ms(&self, coin: &str, interval_secs: u64) -> Option<i64> {
        let sql = format!(
            "SELECT max(t) FROM hl_candles WHERE coin='{coin}' AND interval={interval_secs} FORMAT TSV"
        );
        let r = self.exec(&sql).await.ok()?;
        let v: i64 = r.trim().parse().ok()?;
        if v <= 0 {
            None
        } else {
            Some(v)
        }
    }
}

/// RESUME-SAFE pompalama: CH'den kaldığı yerden devam; yoksa tam pencere.
pub async fn pump_candles_resumable(
    info: &hyperliquid_api::Info,
    ch: &ClickHouseStorage,
    coin: &str,
    interval_secs: u64,
    days: u64,
    request_pause_ms: u64,
    sink: &mut dyn CandleSink,
) -> Result<(usize, bool)> {
    let interval_ms = interval_secs as i64 * 1000;
    // yeni pencere başlangıcı: depolanmış son t'den sonraki mum
    let fresh_from = ch
        .last_candle_ms(coin, interval_secs)
        .await
        .map(|t| t + interval_ms);
    let now = hyperliquid_api::now_ms() as i64;
    let window_start = now - days as i64 * 86_400_000;
    let start = match fresh_from {
        Some(f) if f > window_start => f,
        _ => window_start,
    };
    let resumed = fresh_from.is_some();
    let n = pump_candles_from(info, coin, interval_secs, start, now, request_pause_ms, sink).await?;
    Ok((n, resumed))
}
