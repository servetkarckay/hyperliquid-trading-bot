//! Yüksek hacimli veri akışı yönetimi — tüm aktif coinler için.
//!
//! load_universe(): metaAndAssetCtxs tek çağrıyla 234 coinden delist edilmişleri
//! ve hacim eşiği altındakileri eler, günlük hacme göre sıralar.
//!
//! Ingestor::start(): coinleri N adet WS bağlantısına (shard) böler; her shard
//! kendi coinleri için trades + activeAssetCtx (+ ilk N coine l2Book)
//! aboneliği açar; shard 0 tüm evreni tek abonelikten getiren allMids'i ekler.
//! Kopan shard otomatik yeniden bağlanıp abonelikleri tazeler.
//!
//! StreamEvent (birleşik akış):
//! - Trade  : {coin, px, sz, ts, is_buy}      -> Aggregator
//! - Book   : {coin, bids, asks, ts}          -> yerel defter takibi
//! - Mids   : tüm evren mid haritası          -> tarama/rejim
//! - Ctx    : {funding, OI, mark, oracle, premium, dayVlm} -> MarketCtx
//! translate(): ham WsEvent'i StreamEvent'e çevirir; trades dizisi tek tek
//! iletilir (HİÇ tick kaybolmaz — canlı testte 77->438 tick yakalandı).
use std::collections::HashMap;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use hyperliquid_api::{Config, Info, WsClient};

#[derive(Debug, Clone)]
pub struct IngestorConfig {
    /// WS bağlantı sayısı (coinler buna bölünür)
    pub shard_count: usize,
    /// Günlük hacmi bunun altındaki coinler atlanır (USD); 0 = tümü
    pub min_day_volume_usd: f64,
    /// l2Book aboneliği açılacak coin sayısı (günlük hacme göre ilk N); 0 = kapalı
    pub l2book_top_n: usize,
    /// activeAssetCtx aboneliği: canlı funding/OI/mark/oracle (tüm aktif coinler)
    pub ctx_enabled: bool,
    pub include_trades: bool,
    pub include_all_mids: bool,
    /// Test/limit için coin sayısı tavanı
    pub max_coins: Option<usize>,
}

impl Default for IngestorConfig {
    fn default() -> Self {
        Self {
            shard_count: 4,
            min_day_volume_usd: 0.0,
            l2book_top_n: 30,
            ctx_enabled: true,
            include_trades: true,
            include_all_mids: true,
            max_coins: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoinInfo {
    pub name: String,
    pub asset: u32,
    pub sz_decimals: u32,
    pub day_volume_usd: f64,
}

/// Aktif perp evreni (delist edilenler hariç, hacme göre azalan)
#[derive(Debug, Clone, Default)]
pub struct Universe {
    pub coins: Vec<CoinInfo>,
}

impl Universe {
    pub fn names(&self) -> Vec<String> {
        self.coins.iter().map(|c| c.name.clone()).collect()
    }
    pub fn get(&self, coin: &str) -> Option<&CoinInfo> {
        self.coins.iter().find(|c| c.name == coin)
    }
}

/// Tek tek abonelik yerine birleşik akış event'leri
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Trade { coin: String, px: f64, sz: f64, ts: i64, is_buy: bool },
    Book { coin: String, bids: Vec<(f64, f64)>, asks: Vec<(f64, f64)>, ts: i64 },
    Mids { mids: HashMap<String, f64>, ts: i64 },
    /// WS activeAssetCtx: canlı piyasa bağlamı (funding, OI, mark/oracle...)
    Ctx { coin: String, funding: f64, open_interest: f64, mark_px: f64, oracle_px: f64, premium: f64, day_ntl_vlm: f64, prev_day_px: f64, ts: i64 },
}

/// WS/REST sayı alanları: string veya number gelebilir
pub fn num(v: &Value) -> f64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64()).unwrap_or(0.0)
}

/// metaAndAssetCtxs TEK çağrıyla aktif evreni çeker (delist hariç, hacme göre sıralı)
pub async fn load_universe(info: &Info, cfg: &IngestorConfig) -> Result<Universe> {
    let (meta, ctxs) = info.meta_and_asset_ctxs().await?;
    let mut coins: Vec<CoinInfo> = Vec::with_capacity(meta.universe.len());
    for (i, m) in meta.universe.iter().enumerate() {
        if m.isDelisted.unwrap_or(false) {
            continue;
        }
        let vol: f64 = ctxs.get(i).map(|c| c.dayNtlVlm.parse().unwrap_or(0.0)).unwrap_or(0.0);
        if vol < cfg.min_day_volume_usd {
            continue;
        }
        coins.push(CoinInfo {
            name: m.name.clone(),
            asset: i as u32,
            sz_decimals: m.szDecimals,
            day_volume_usd: vol,
        });
    }
    coins.sort_by(|a, b| b.day_volume_usd.total_cmp(&a.day_volume_usd));
    if let Some(max) = cfg.max_coins {
        coins.truncate(max);
    }
    Ok(Universe { coins })
}

pub struct Ingestor {
    pub cfg: IngestorConfig,
    pub universe: Universe,
}

impl Ingestor {
    pub async fn new(cfg: Config, icfg: IngestorConfig) -> Result<Self> {
        let info = Info::new(cfg)?;
        let universe = load_universe(&info, &icfg).await?;
        tracing::info!(
            "evren: {} aktif coin (min hacim ${:.0}, l2book ilk {})",
            universe.coins.len(), icfg.min_day_volume_usd, icfg.l2book_top_n
        );
        Ok(Self { cfg: icfg, universe })
    }

    /// Shard'lı WS bağlantılarını başlatır; birleşik StreamEvent akışı döner.
    /// Her shard kopunca kendi kendine yeniden bağlanır (WsClient.run);
    /// ham WsEvent'ler tek noktada StreamEvent'e çevrilir.
    pub fn start(&self, api_cfg: Config) -> mpsc::Receiver<StreamEvent> {
        let (tx_out, rx) = mpsc::channel(8192);
        let (tx_raw, mut rx_raw) = mpsc::channel(8192);
        let shards = self.cfg.shard_count.max(1);
        let top_n = self.cfg.l2book_top_n;

        for shard in 0..shards {
            let mut subs: Vec<Value> = Vec::new();
            if shard == 0 && self.cfg.include_all_mids {
                subs.push(json!({ "type": "allMids" }));
            }
            for (i, coin) in self.universe.coins.iter().enumerate() {
                if i % shards != shard {
                    continue;
                }
                if self.cfg.include_trades {
                    subs.push(json!({ "type": "trades", "coin": coin.name }));
                }
                if self.cfg.ctx_enabled {
                    // canlı funding/OI/mark/oracle/premium (stratejiler 08, 09, 10)
                    subs.push(json!({ "type": "activeAssetCtx", "coin": coin.name }));
                }
                if top_n > 0 && i < top_n {
                    subs.push(json!({ "type": "l2Book", "coin": coin.name }));
                }
            }
            let tx_raw = tx_raw.clone();
            let cfg = api_cfg.clone();
            tracing::info!("shard {shard}: {} abonelik", subs.len());
            tokio::spawn(async move {
                let ws = WsClient::new(cfg);
                if let Err(e) = ws.run(subs, tx_raw).await {
                    tracing::error!("shard sonlandı: {e}");
                }
            });
        }
        // tx_raw'un son kopyasını bırak -> tüm shardlar bittiğinde pump da biter
        drop(tx_raw);

        // Pump: WsEvent -> StreamEvent (trades dizisi tek tek iletilir)
        tokio::spawn(async move {
            while let Some(ev) = rx_raw.recv().await {
                for se in Self::translate(&ev) {
                    if tx_out.send(se).await.is_err() {
                        return; // tüketici kapandı
                    }
                }
            }
        });
        rx
    }

    /// Ham WsEvent -> birleşik StreamEvent'ler (trades dizisi -> her tick ayrı).
    /// Boş vec: ilgisiz/bozuk mesaj.
    pub fn translate(ev: &hyperliquid_api::websocket::WsEvent) -> Vec<StreamEvent> {
        match ev.channel.as_str() {
            "trades" => ev
                .data
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| {
                            Some(StreamEvent::Trade {
                                coin: t["coin"].as_str()?.to_string(),
                                px: t["px"].as_str()?.parse().ok()?,
                                sz: t["sz"].as_str()?.parse().ok()?,
                                ts: t["time"].as_i64().unwrap_or(0),
                                is_buy: t["side"] == "B",
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            "l2Book" => {
                let parse = |lv: &Value| -> Vec<(f64, f64)> {
                    lv.as_array().map(|a| {
                        a.iter()
                            .filter_map(|l| {
                                Some((
                                    l["px"].as_str()?.parse().ok()?,
                                    l["sz"].as_str()?.parse().ok()?,
                                ))
                            })
                            .collect()
                    }).unwrap_or_default()
                };
                match ev.data["levels"].as_array() {
                    Some(levels) if levels.len() == 2 => vec![StreamEvent::Book {
                        coin: ev.data["coin"].as_str().unwrap_or("").to_string(),
                        bids: parse(&levels[0]),
                        asks: parse(&levels[1]),
                        ts: ev.data["time"].as_i64().unwrap_or(0),
                    }],
                    _ => vec![],
                }
            }
            "allMids" => match ev.data["mids"].as_object() {
                Some(m) => vec![StreamEvent::Mids {
                    mids: m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.parse().ok()?))).collect(),
                    ts: hyperliquid_api::now_ms() as i64,
                }],
                None => vec![],
            },
            "activeAssetCtx" => {
                let ctx = &ev.data["ctx"];
                if ctx.is_null() {
                    return vec![];
                }
                vec![StreamEvent::Ctx {
                    coin: ev.data["coin"].as_str().unwrap_or("").to_string(),
                    funding: num(&ctx["funding"]),
                    open_interest: num(&ctx["openInterest"]),
                    mark_px: num(&ctx["markPx"]),
                    oracle_px: num(&ctx["oraclePx"]),
                    premium: num(&ctx["premium"]),
                    day_ntl_vlm: num(&ctx["dayNtlVlm"]),
                    prev_day_px: num(&ctx["prevDayPx"]),
                    ts: hyperliquid_api::now_ms() as i64,
                }]
            }
            _ => vec![],
        }
    }
}
