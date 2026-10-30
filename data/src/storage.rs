//! STORAGE — kalıcılık soyutlaması (trait + gerçekleştirmeler).
//!
//! 1. `Storage` trait: trades / candles / ctx / funding kaydetme-okuma.
//! 2. `MemoryStorage`: test ve düşük-konfig fallback.
//! 3. `ClickHouseStorage` (CLICKHOUSE_URL): tarihsel arşiv — backfill_job
//!    ile kullanılır (hl_trades / hl_candles / hl_funding / hl_ctx /
//!    hl_strategy_performance). Canlı bot bu katmanı KULLANMAZ; canlı state
//!    JSON dosyalarında tutulur.
//!
//! URL'ler boşsa sistem Memory ile ÇALIŞMAYA devam eder (indirimli mod —
//! arşiv/backtest kaybı loglanır).
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
        Ok(Self {
            url,
            http: reqwest::Client::new(),
        })
    }

    /// ClickHouse HTTP çağrısı: 200 dışı durumlarda gövdeyi şhata taşı.
    /// (CH, INSERT hatasında 500 + hata metni döner — gövdeyi "başarı"
    /// gibi Ok etmek etkisizleştirmektir.)
    pub async fn exec(&self, sql: &str) -> Result<String> {
        let resp = self
            .http
            .post(&self.url)
            .body(sql.to_string())
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            let snippet: String = body.chars().take(300).collect();
            tracing::warn!("[STORAGE] ClickHouse HTTP {status}: {snippet}");
            anyhow::bail!("ClickHouse HTTP {status}: {snippet}");
        }
        Ok(body)
    }

    /// Tabloları oluştur (varsa no-op) — ClickHouse kullanımı için public.
    pub async fn ensure_tables(&self) -> Result<()> {
        self.exec("CREATE TABLE IF NOT EXISTS hl_trades (coin String, px Float64, sz Float64, ts Int64, is_buy Bool) ENGINE = MergeTree ORDER BY (coin, ts)").await?;
        self.exec("CREATE TABLE IF NOT EXISTS hl_candles (coin String, interval UInt64, t Int64, o Float64, h Float64, l Float64, c Float64, v Float64, n UInt64) ENGINE = ReplacingMergeTree ORDER BY (coin, interval, t)").await?;
        self.exec("CREATE TABLE IF NOT EXISTS hl_funding (coin String, ts Int64, fundingRate Float64, premium Float64 DEFAULT 0) ENGINE = ReplacingMergeTree ORDER BY (coin, ts)").await?;
        self.exec("CREATE TABLE IF NOT EXISTS hl_ctx (coin String, ts Int64, funding Float64, openInterest Float64, markPx Float64, oraclePx Float64, premium Float64, dayNtlVlm Float64, prevDayPx Float64) ENGINE = ReplacingMergeTree ORDER BY (coin, ts)").await?;
        self.exec("CREATE TABLE IF NOT EXISTS hl_strategy_performance (strategy String, interval UInt64, sample String, ts Int64, trades UInt64, winRate Float64, profitFactor Float64, totalReturn Float64, maxDrawdown Float64, sharpe Float64, avgBars Float64) ENGINE = ReplacingMergeTree ORDER BY (strategy, interval, sample, ts)").await?;
        Ok(())
    }

    /// İmza veri ekleme — geçici hata için SINIRLI retry (3 deneme),
    /// son hata `Err` olarak üst katman taşınır (sessiz başarı YOK).
    pub async fn exec_insert(&self, sql: &str) -> Result<()> {
        let last_err = self.exec(sql).await.inspect_err(|e| {
            tracing::warn!("[STORAGE] insert denemesi başarısız: {e}");
        });
        match last_err {
            Ok(_) => Ok(()),
            Err(e) => {
                tokio::time::sleep(std::time::Duration::from_millis(INSERT_RETRY_MS)).await;
                tracing::warn!("[STORAGE] insert retry (2/{}): {}", INSERT_RETRIES, e);
                match self.exec(sql).await {
                    Ok(_) => {
                        tracing::info!("[STORAGE] insert retry başarılı (2/{})", INSERT_RETRIES);
                        Ok(())
                    }
                    Err(e2) => {
                        tracing::warn!("[STORAGE] insert retry başarısız: {e2}");
                        Err(anyhow::anyhow!(
                            "INSERT {INSERT_RETRIES}/{INSERT_RETRIES} denemeden sonra hata: {e2}"
                        ))
                    }
                }
            }
        }
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
            row
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
        // SAYI SÖZLEŞMESİ: fundingRate parse edilemeyen (bozuk) satır
        // reddedilir — sahte 0.0 yazılmaz. premium HL'de meşru None'dır;
        // satırda premium ALANI YAZILMAZ (DB DEFAULT 0, tablo şemasında
        // beyanlı) — unparseable premium da "yok" muamelesi görür.
        let mut kept = 0usize;
        let mut rejected = 0usize;
        let rows: Vec<String> = hist
            .iter()
            .filter_map(|e| {
                let rate = e.fundingRate.parse::<f64>().ok().filter(|x| x.is_finite());
                let premium = e
                    .premium
                    .as_ref()
                    .and_then(|p| p.parse::<f64>().ok())
                    .filter(|x| x.is_finite());
                let Some(rate) = rate else {
                    rejected += 1;
                    tracing::debug!(
                        "[STORAGE] funding satırı reddedildi: rate geçersiz ({coin} @{})",
                        e.time
                    );
                    return None;
                };
                kept += 1;
                Some(match premium {
                    Some(p) => serde_json::json!({
                        "coin": coin, "ts": e.time,
                        "fundingRate": rate, "premium": p
                    })
                    .to_string(),
                    None => serde_json::json!({
                        "coin": coin, "ts": e.time, "fundingRate": rate
                    })
                    .to_string(),
                })
            })
            .collect();
        if rejected > 0 {
            tracing::warn!("[STORAGE] funding: {rejected} bozuk satır reddedildi ({coin})");
        }
        if rows.is_empty() {
            anyhow::bail!("[STORAGE] funding: geçerli satır yok ({coin}, {rejected} reddedildi)");
        }
        self.exec_insert(&format!(
            "INSERT INTO hl_funding FORMAT JSONEachRow\n{}",
            rows.join("\n")
        ))
        .await?;
        Ok(kept)
    }
}

#[async_trait]
impl Storage for ClickHouseStorage {
    async fn save_ctx(&mut self, ctxs: &[crate::market_ctx::CtxState]) -> Result<usize> {
        if ctxs.is_empty() {
            return Ok(0);
        }
        let rows: Vec<String> = ctxs
            // SAYI SÖZLEŞMESİ: finite olmayan ctx alanı (NaN/Inf) sahte
            // sıfıra dönüşmez — o satır reddedilir.
            .iter()
            .filter_map(|c| {
                let ok = c.ts != 0
                    && c.funding.is_finite()
                    && c.open_interest.is_finite()
                    && c.mark_px.is_finite()
                    && c.oracle_px.is_finite()
                    && c.premium.is_finite()
                    && c.day_ntl_vlm.is_finite()
                    && c.prev_day_px.is_finite();
                if !ok {
                    return None;
                }
                Some(
                    serde_json::json!({
                        "coin": c.coin, "ts": c.ts, "funding": c.funding,
                        "openInterest": c.open_interest, "markPx": c.mark_px,
                        "oraclePx": c.oracle_px, "premium": c.premium,
                        "dayNtlVlm": c.day_ntl_vlm, "prevDayPx": c.prev_day_px
                    })
                    .to_string(),
                )
            })
            .collect();
        let rejected = ctxs.len() - rows.len();
        if rejected > 0 {
            tracing::warn!("[STORAGE] ctx: {rejected} bozuk satır reddedildi");
        }
        if rows.is_empty() {
            anyhow::bail!("[STORAGE] ctx: geçerli satır yok ({rejected} reddedildi)");
        }
        self.exec_insert(&format!(
            "INSERT INTO hl_ctx FORMAT JSONEachRow\n{}",
            rows.join("\n")
        ))
        .await?;
        Ok(rows.len())
    }

    async fn save_trades(&mut self, trades: &[TradeRecord]) -> Result<usize> {
        if trades.is_empty() {
            return Ok(0);
        }
        // SAYI SÖZLEŞMESİ: px/sz finite olmayan trade reddedilir
        let rows: Vec<String> = trades
            .iter()
            .filter_map(|t| {
                if t.ts == 0 || !t.px.is_finite() || !t.sz.is_finite() {
                    return None;
                }
                Some(
                    serde_json::json!({
                        "coin": t.coin, "px": t.px, "sz": t.sz, "ts": t.ts, "is_buy": t.is_buy
                    })
                    .to_string(),
                )
            })
            .collect();
        let rejected = trades.len() - rows.len();
        if rejected > 0 {
            tracing::warn!("[STORAGE] trades: {rejected} bozuk satır reddedildi");
        }
        if rows.is_empty() {
            anyhow::bail!("[STORAGE] trades: geçerli satır yok");
        }
        let sql = format!(
            "INSERT INTO hl_trades FORMAT JSONEachRow\n{}",
            rows.join("\n")
        );
        self.exec(&sql).await?;
        Ok(rows.len())
    }

    async fn save_candles(
        &mut self,
        coin: &str,
        interval_secs: u64,
        candles: &[Candle],
    ) -> Result<usize> {
        if candles.is_empty() {
            return Ok(0);
        }
        // SAYI SÖZLEŞMESİ: finite olmayan mum (NaN/Inf) 0.0'a DÖNÜŞMEZ —
        // o satır reddedilir; t<=0 (zaman damgası yok) da reddedilir.
        let mut kept = 0usize;
        let mut rejected = 0usize;
        let rows: Vec<String> = candles
            .iter()
            .filter_map(|c| {
                let ok = c.t > 0
                    && c.o.is_finite()
                    && c.h.is_finite()
                    && c.l.is_finite()
                    && c.c.is_finite()
                    && c.v.is_finite()
                    && c.h >= c.l
                    && c.h >= c.o.max(c.c)
                    && c.l <= c.o.min(c.c)
                    && c.h > 0.0; // gerçek mumda yüksek fiyat 0 olamaz
                if !ok {
                    rejected += 1;
                    return None;
                }
                kept += 1;
                Some(
                    serde_json::json!({
                        "coin": coin, "interval": interval_secs, "t": c.t,
                        "o": c.o, "h": c.h, "l": c.l, "c": c.c, "v": c.v, "n": c.n
                    })
                    .to_string(),
                )
            })
            .collect();
        if rejected > 0 {
            tracing::warn!("[STORAGE] candles: {rejected} bozuk satır reddedildi ({coin})");
        }
        if rows.is_empty() {
            anyhow::bail!("[STORAGE] candles: geçerli satır yok ({coin}, {rejected} reddedildi)");
        }
        self.exec_insert(&format!(
            "INSERT INTO hl_candles FORMAT JSONEachRow\n{}",
            rows.join("\n")
        ))
        .await?;
        Ok(kept)
    }

    async fn load_candles(
        &mut self,
        coin: &str,
        interval_secs: u64,
        limit: usize,
    ) -> Result<Vec<Candle>> {
        let sql = format!(
            "SELECT t, o, h, l, c, v, n FROM hl_candles WHERE coin='{coin}' AND interval={interval_secs} ORDER BY t DESC LIMIT {limit} FORMAT JSONEachRow"
        );
        let body = self.exec(&sql).await?;
        let mut out = Vec::new();
        let mut rejected = 0usize;
        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // SAYI SÖZLEŞMESİ: t yok / non-finite / h<l → satır reddedilir;
            // 0.0 mum ÜRETİLMEZ (sinyal zehirine karşı)
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                rejected += 1;
                continue;
            };
            let (Some(t), Some(o), Some(h), Some(l), Some(cc), Some(vv)) = (
                v["t"].as_i64(),
                v["o"].as_f64(),
                v["h"].as_f64(),
                v["l"].as_f64(),
                v["c"].as_f64(),
                v["v"].as_f64(),
            ) else {
                rejected += 1;
                continue;
            };
            if !o.is_finite()
                || !h.is_finite()
                || !l.is_finite()
                || !cc.is_finite()
                || !vv.is_finite()
                || h < l
                || t <= 0
                || h <= 0.0
            // gerçek mumda yüksek fiyat 0 olamaz (bozuk satır)
            {
                rejected += 1;
                continue;
            }
            out.push(Candle {
                t,
                o,
                h,
                l,
                c: cc,
                v: vv,
                n: v["n"].as_u64().unwrap_or(1),
                closed: true,
            });
        }
        if rejected > 0 {
            tracing::warn!("[STORAGE] load_candles: {rejected} bozuk satır reddedildi ({coin})");
        }
        if out.is_empty() && !body.trim().is_empty() {
            anyhow::bail!(
                "[STORAGE] load_candles: satırların TÜMÜ bozuk ({coin}) — sahte mum üretilmez"
            );
        }
        out.reverse();
        Ok(out)
    }

    async fn trade_count(&self) -> Result<u64> {
        // TSV: düz sayı döner; parse edilemiyorsa SAHTE 0 döndürülmez
        let r = self
            .exec("SELECT count() FROM hl_trades FORMAT TSV")
            .await?;
        let trimmed = r.trim();
        trimmed.parse::<u64>().map_err(|e| {
            anyhow::anyhow!("[STORAGE] trade_count parse edilemedi ('{trimmed}'): {e}")
        })
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

#[cfg(test)]
mod storage_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Mini ClickHouse HTTP mock: `count` kadar isteğe `status` yanıt
    /// döner, her istek gövdesini `captured`'a ekler (satır reddi /
    /// hata yayılımı testleri ağsız doğrulanır).
    async fn spawn_mock(
        status: u16,
        count: usize,
        resp_body: &str,
        captured: Arc<Mutex<Vec<String>>>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = resp_body.to_string();
        tokio::spawn(async move {
            for _ in 0..count {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break None;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break Some(p);
                        }
                        if buf.len() > 1 << 20 {
                            break None;
                        }
                    };
                    let Some(hp) = header_end else { continue };
                    let header = String::from_utf8_lossy(&buf[..hp]).to_lowercase();
                    let content_len: usize = header
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse().unwrap_or(0))
                        .unwrap_or(0);
                    while buf.len() < hp + 4 + content_len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let seg = buf
                        .get(hp + 4..(hp + 4 + content_len).min(buf.len()))
                        .map(|v| String::from_utf8_lossy(v).to_string())
                        .unwrap_or_default();
                    captured.lock().unwrap().push(seg);
                    let resp = format!(
                        "HTTP/1.1 {status} T\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                }
            }
        });
        format!("http://{addr}")
    }

    fn ch(url: &str) -> ClickHouseStorage {
        ClickHouseStorage {
            url: url.into(),
            http: reqwest::Client::new(),
        }
    }

    /// INSERT hata propagation: ClickHouse 500 dönerse exec_insert Err
    /// döner; bounded retry (2 deneme) uygulanır — sessiz başarı YOK.
    #[tokio::test]
    async fn insert_500_err_propagate_bounded_retry() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_mock(500, 2, "DB malformed", captured.clone()).await;
        let st = ch(&url);
        let r = st.exec_insert("INSERT INTO x FORMAT JSONEachRow\n{}").await;
        assert!(r.is_err(), "500 → Err beklenirdi");
        assert_eq!(captured.lock().unwrap().len(), 2, "bounded: 2 deneme");
    }

    /// Valid mum insert: 200 → Ok(len), gövde satır sayısı korunur.
    #[tokio::test]
    async fn valid_candle_insert_success() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_mock(200, 1, "", captured.clone()).await;
        let mut st = ch(&url);
        let c = Candle {
            t: 1000,
            o: 100.0,
            h: 101.0,
            l: 99.0,
            c: 100.5,
            v: 10.0,
            n: 1,
            closed: true,
        };
        let r = st.save_candles("BTC", 3600, &[c]).await.unwrap();
        assert_eq!(r, 1);
        let body = captured.lock().unwrap()[0].clone();
        assert!(body.contains("\"t\":1000") && body.contains("hl_candles"));
    }

    /// Bozuk mum: NaN close 0.0'a DÖNÜŞMEZ, satır reddedilir; valid
    /// kalan kalır (kept=1).
    #[tokio::test]
    async fn bozuk_mum_satiri_reddedilir() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_mock(200, 1, "", captured.clone()).await;
        let mut st = ch(&url);
        let c1 = Candle {
            t: 1000,
            o: 100.0,
            h: 101.0,
            l: 99.0,
            c: 100.5,
            v: 10.0,
            n: 1,
            closed: true,
        };
        let c2 = Candle {
            t: 2000,
            o: 100.0,
            h: 101.0,
            l: 99.0,
            c: f64::NAN,
            v: 10.0,
            n: 1,
            closed: true,
        };
        let r = st.save_candles("BTC", 3600, &[c1, c2]).await.unwrap();
        assert_eq!(r, 1, "bozuk satır düşer, valid kalır");
        let body = captured.lock().unwrap()[0].clone();
        assert_eq!(body.lines().skip(1).count(), 1, "gövdede 1 veri satırı");
        assert!(!body.contains("2000"), "reddedilen bar gövdede değil");
    }

    /// TÜM satırlar bozuk → error (sessiz başarı YOK, sunucuya istek bile gitmez).
    #[tokio::test]
    async fn tum_satirlar_bozuk_err() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_mock(200, 0, "", captured.clone()).await;
        let mut st = ch(&url);
        let c = Candle {
            t: 1000,
            o: f64::NAN,
            h: 101.0,
            l: 99.0,
            c: 100.5,
            v: 10.0,
            n: 1,
            closed: true,
        };
        let r = st.save_candles("BTC", 3600, &[c]).await;
        assert!(r.is_err());
        assert!(
            captured.lock().unwrap().is_empty(),
            "geçersiz toplu veri DB'ye gönderilmez"
        );
    }

    /// Bozuk funding satırı reddedilir; premium None satırda üretmez
    /// (DEFAULT 0 şemanın beyanıdır; 0.0 "sahte veri" değil, yokluk).
    #[tokio::test]
    async fn bozuk_funding_satiri_reddedilir() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_mock(200, 1, "", captured.clone()).await;
        let st = ch(&url);
        let bad = hyperliquid_api::types::FundingEntry {
            coin: "BTC".into(),
            fundingRate: "abc".into(),
            premium: Some("0.1".into()),
            time: 1000,
        };
        let good = hyperliquid_api::types::FundingEntry {
            coin: "BTC".into(),
            fundingRate: "0.0001".into(),
            premium: None,
            time: 2000,
        };
        let r = st.save_funding("BTC", &[bad, good]).await.unwrap();
        assert_eq!(r, 1, "1 valid satır");
        let body = captured.lock().unwrap()[0].clone();
        assert_eq!(body.lines().skip(1).count(), 1);
        assert!(body.contains("\"fundingRate\":0.0001"), "{body}");
        assert!(
            !body.contains("\"premium\""),
            "None premium sahte değerle doldurulmaz: {body}"
        );
    }

    /// ctx satırında NaN alan: satır reddedilir.
    #[tokio::test]
    async fn bozuk_ctx_satiri_reddedilir() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_mock(200, 1, "", captured.clone()).await;
        let mut st = ch(&url);
        let good = crate::market_ctx::CtxState {
            coin: "BTC".into(),
            funding: 0.0001,
            open_interest: 1.0,
            mark_px: 100.0,
            oracle_px: 99.0,
            premium: 0.0,
            day_ntl_vlm: 1.0,
            prev_day_px: 0.0,
            ts: 1000,
        };
        let bad = crate::market_ctx::CtxState {
            open_interest: f64::NAN,
            ..good.clone()
        };
        let r = st.save_ctx(&[good, bad]).await.unwrap();
        assert_eq!(r, 1);
        assert_eq!(captured.lock().unwrap()[0].lines().skip(1).count(), 1);
    }

    /// load_candles: bozuk JSON/NaN satır reddedilir, valid okunur; TÜMÜ
    /// bozuksa error (0.0 mum GÖRMEZ).
    #[tokio::test]
    async fn load_candles_bozuk_satir_reddedilir() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let body = concat!(
            "{\"t\":1000,\"o\":100.0,\"h\":101.0,\"l\":99.0,\"c\":100.5,\"v\":10,\"n\":1}\n",
            "{\"t\":\"x\"}\n",
            "{\"t\":2000,\"o\":0.0,\"h\":0.0,\"l\":0.0,\"c\":0.0,\"v\":0,\"n\":1}\n"
        );
        let url = spawn_mock(200, 1, body, captured.clone()).await;
        let mut st = ch(&url);
        let c = st.load_candles("BTC", 3600, 10).await.unwrap();
        assert_eq!(c.len(), 1, "corrupt 2 satır reddildi: {c:?}");
        assert_eq!(c[0].t, 1000);
        // geçerli girdiyi "koru": satır sayısı en az 1 olmalı (valid)
        assert_eq!(c[0].c, 100.5);
    }

    /// trade_count: istemci tarafında parse edilemeyen TSV → Err (0 sahte değil).
    #[tokio::test]
    async fn trade_count_parse_err() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_mock(200, 1, "not-a-number\n", captured.clone()).await;
        let st = ch(&url);
        let r = st.trade_count().await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn trade_count_parse_ok() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_mock(200, 1, "42\n", captured.clone()).await;
        let st = ch(&url);
        assert_eq!(st.trade_count().await.unwrap(), 42);
    }
}
