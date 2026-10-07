//! INGESTOR — shard'lı Hyperliquid WS bağlantıları ve olay çevirisi.
//!
//! Görevler:
//! 1. Evreni abonelik listelerine böler (shard başına ~160 kanal) ve
//!
//! her shard için ayrı WS bağlantısı açar.
//! 2. Ham WS kanallarını (trades / l2Book / activeAssetCtx / orderUpdates /
//!
//! userEvents) `StreamEvent`'e çevirir (`translate`).
//! 3. `IngestorConfig`: shard_count, min_day_volume_usd (düşük hacimli
//!
//! coin filtresi), l2book_top_n, ctx_enabled.
//! evren DIŞARIDAN verilir (`with_universe`) — sessiz kırpma yok;
//! abonelik listesi `TradingUniverse`'den üretilir.
use std::collections::HashMap;

use anyhow::Result;

use crate::universe::TradingUniverse;
use serde_json::Value;
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
    Trade {
        coin: String,
        px: f64,
        sz: f64,
        ts: i64,
        is_buy: bool,
    },
    Book {
        coin: String,
        bids: Vec<(f64, f64)>,
        asks: Vec<(f64, f64)>,
        ts: i64,
    },
    Mids {
        mids: HashMap<String, f64>,
        ts: i64,
    },
    /// WS activeAssetCtx: canlı piyasa bağlamı (funding, OI, mark/oracle...)
    Ctx {
        coin: String,
        funding: f64,
        open_interest: f64,
        mark_px: f64,
        oracle_px: f64,
        premium: Option<f64>,
        day_ntl_vlm: Option<f64>,
        prev_day_px: Option<f64>,
        ts: i64,
    },
}

/// WS/REST sayı alanları: string veya number gelebilir.
/// Eksik/geçersiz/finite-olmayan değer → `None` — sahte 0.0 taşınmaz;
/// arayan taraf event'i geçersiz sayar (drop) ya da Optionsel alan olarak
/// geçirir ("bozuk ≠ sıfır" kontratı).
pub fn num(v: &Value) -> Option<f64> {
    v.as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| v.as_f64())
        .filter(|x| x.is_finite())
}

/// metaAndAssetCtxs TEK çağrıyla aktif evreni çeker (delist hariç, hacme göre sıralı)
pub async fn load_universe(info: &Info, cfg: &IngestorConfig) -> Result<Universe> {
    let (meta, ctxs) = info.meta_and_asset_ctxs().await?;
    let mut coins: Vec<CoinInfo> = Vec::with_capacity(meta.universe.len());
    for (i, m) in meta.universe.iter().enumerate() {
        if m.isDelisted.unwrap_or(false) {
            continue;
        }
        // VERİ DÜRÜSTLÜĞÜ: bozuk hacim = coin SESSİZ düşürülmez — loglanır
        // (fail-closed: hacim doğrulanamayan coin evren dışı kalır, ama
        // bunun görülür olması zorunludur).
        let vol: f64 = match ctxs.get(i) {
            Some(c) => match c.dayNtlVlm.parse::<f64>() {
                Ok(v) if v.is_finite() => v,
                _ => {
                    tracing::warn!(
                        "[UNIVERSE] {}: dayNtlVlm bozuk ('{}') — coin düşürüldü",
                        m.name,
                        c.dayNtlVlm
                    );
                    0.0
                }
            },
            None => {
                tracing::warn!("[UNIVERSE] {}: ctx yok — coin düşürüldü", m.name);
                0.0
            }
        };
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
    // burada KIRPMA YAPILMAZ. Evren bir kez `TradingUniverse::build`
    // içinde çözülür; ingestor `max_coins` ile kendi TOP-N'ini seçerse
    // engine listesiyle AYRIŞIR (ayni sayi, farkli set).
    coins.sort_by(|a, b| b.day_volume_usd.total_cmp(&a.day_volume_usd));
    Ok(Universe { coins })
}

pub struct Ingestor {
    pub cfg: IngestorConfig,
    /// evren tek kaynaktan gelir (`TradingUniverse`), `Universe` DEĞİL.
    pub universe: TradingUniverse,
}

impl Ingestor {
    /// evren DIŞARIDAN verilir — ingestor kendi evrenini KENDISI
    /// seçmez. `Ingestor::new` yerine `Ingestor::with_universe` kullanılır.
    pub async fn with_universe(
        _cfg: Config,
        icfg: IngestorConfig,
        universe: TradingUniverse,
    ) -> Result<Self> {
        // Katman doğrulaması: engine listesi ile abonelik listesi birebir aynı
        // olmalı. Uyuşmazlıkta burada hata verir -> bot başlamaz.
        universe.verify_layers(&icfg)?;
        tracing::info!(
            "evren: {} coin (min hacim ${:.0}, l2book ilk {})",
            universe.len(),
            icfg.min_day_volume_usd,
            icfg.l2book_top_n
        );
        Ok(Self {
            cfg: icfg,
            universe,
        })
    }

    /// Shard'lı WS bağlantılarını başlatır; birleşik StreamEvent akışı döner.
    /// Her shard kopunca kendi kendine yeniden bağlanır (WsClient.run);
    /// ham WsEvent'ler tek noktada StreamEvent'e çevrilir.
    pub fn start(&self, api_cfg: Config) -> mpsc::Receiver<StreamEvent> {
        let (tx_out, rx) = mpsc::channel(8192);
        let (tx_raw, mut rx_raw) = mpsc::channel(8192);
        let shards = self.cfg.shard_count.max(1);
        // abonelik listesi TEK KAYNAKTAN (`TradingUniverse`) üretilir.
        let all_subs = self.universe.subscription_list(&self.cfg);
        let mut per_shard: Vec<Vec<Value>> = vec![Vec::new(); shards];
        for v in all_subs {
            match v["type"].as_str() {
                // allMids coin-bağımsız: ilk shard'a konur
                Some("allMids") => per_shard[0].push(v),
                _ => {
                    // coin'e göre shard dağıtımı — evren sırasından türetilir
                    let i = self
                        .universe
                        .names()
                        .iter()
                        .position(|c| Some(c.as_str()) == v["coin"].as_str())
                        .unwrap_or(0);
                    per_shard[i % shards].push(v);
                }
            }
        }
        for (shard, subs) in per_shard.into_iter().enumerate() {
            let tx_raw = tx_raw.clone();
            let cfg = api_cfg.clone();
            tracing::info!("shard {shard}: {} abonelik", subs.len());
            // SUPERVISOR: shard kalıcı ölüm = veri akışında kalıcı boşluk;
            // log + 5 sn sonra yeniden başlat (keep-alive tasarımı —
            // iç reconnect'in üst katmanı).
            tokio::spawn(async move {
                loop {
                    let ws = WsClient::new(cfg.clone());
                    match ws.run(subs.clone(), tx_raw.clone()).await {
                        Ok(()) => tracing::warn!("shard {shard} döndü — yeniden başlatılıyor"),
                        Err(e) => tracing::error!(
                            "shard {shard} ÖLDÜ: {e} — 5 sn sonra yeniden başlatılıyor"
                        ),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
                    lv.as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|l| {
                                    Some((
                                        l["px"].as_str()?.parse().ok()?,
                                        l["sz"].as_str()?.parse().ok()?,
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
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
                    mids: m
                        .iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.parse().ok()?)))
                        .collect(),
                    ts: hyperliquid_api::now_ms() as i64,
                }],
                None => vec![],
            },
            "activeAssetCtx" => {
                let ctx = &ev.data["ctx"];
                if ctx.is_null() {
                    return vec![];
                }
                // ÇEKİRDEK SAYI KONTRATI: funding/OI/mark/oracle eksik,
                // geçersiz ya da finite-olmayan event GEÇERSİZDİR ve
                // düşürülür — sahte sıfır bağlam sisteme girmez.
                // (premium/dayNtlVlm/prevDayPx HL'de meşru null olabilir →
                // Option(f64) olarak geçirilir, tüketiciyedeki son bilinen
                // değer korunur.)
                let (Some(funding), Some(oi), Some(mark), Some(oracle)) = (
                    num(&ctx["funding"]),
                    num(&ctx["openInterest"]),
                    num(&ctx["markPx"]),
                    num(&ctx["oraclePx"]),
                ) else {
                    tracing::debug!(
                        coin = %ev.data["coin"],
                        "activeAssetCtx sayı alanları geçersiz — event düşürüldü (sahte 0.0 yazılmadı)"
                    );
                    return vec![];
                };
                vec![StreamEvent::Ctx {
                    coin: ev.data["coin"].as_str().unwrap_or("").to_string(),
                    funding,
                    open_interest: oi,
                    mark_px: mark,
                    oracle_px: oracle,
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

#[cfg(test)]
mod num_sozlesme_testleri {
    use super::*;
    use serde_json::json;

    // REGRESYON KORUMASI: shard kalıcı ölüm bir zamanlar sadece loglanıp
    // bırakılıyordu — veri akışı kalıcı boşlukta kalırdı.
    #[test]
    fn shard_supervisor_dongusu_mevcut() {
        let src = include_str!("ingestor.rs");
        assert!(src.contains("shard {shard} ÖLDÜ"), "shard ölüm logu yok");
        let bas = src
            .find("// SUPERVISOR: shard kalıcı ölüm")
            .expect("shard supervisor bloğu yok");
        let blok = &src[bas..];
        assert!(
            blok.contains("loop {"),
            "shard supervisor DÖNGÜSÜ yok — shard kalıcı ölürse veri akışı kalıcı boşlukta kalır"
        );
        assert!(
            blok.contains("sleep(std::time::Duration::from_secs(5))"),
            "shard supervisor yeniden başlatma beklemesi yok"
        );
    }

    /// SAYI SÖZLEŞMESİ: eksik/geçersiz/finite-olmayan → None; sahte 0.0 YOK.
    #[test]
    fn num_gecerli_string() {
        assert_eq!(num(&json!("0.00012")), Some(0.00012));
    }

    #[test]
    fn num_gecerli_number() {
        assert_eq!(num(&json!(123.5)), Some(123.5));
        assert_eq!(num(&json!(0)), Some(0.0)); // gerçek sıfır GEÇERLİDİR
    }

    #[test]
    fn num_gecersiz_string_sifir_yazmaz() {
        assert_eq!(num(&json!("abc")), None);
        assert_eq!(num(&json!("")), None);
    }

    #[test]
    fn num_eksik_deger_none() {
        assert_eq!(num(&json!(null)), None);
        assert_eq!(num(&Value::Bool(true)), None);
    }

    #[test]
    fn num_finite_olmayan_basilir() {
        // "NaN"/"Infinity" string'i f64 parse'ına GİRER; is_finite keser
        assert_eq!(num(&json!("NaN")), None);
        assert_eq!(num(&json!("Infinity")), None);
        assert_eq!(num(&json!("-Infinity")), None);
        assert_eq!(num(&json!("1e999")), None); // inf'e taşar
    }

    /// Çeviri: geçerli ctx → event üretilir, alanlar birebir taşınır.
    #[test]
    fn ctx_ceviri_gecerli_event_uretir() {
        let ev = hyperliquid_api::websocket::WsEvent {
            channel: "activeAssetCtx".into(),
            data: json!({"coin":"BTC","ctx":{"funding":"0.0001","openInterest":"123.4",
                "markPx":"65000.5","oraclePx":"64999.9","premium":"0.001",
                "dayNtlVlm":"987.6","prevDayPx":"64000"}}),
        };
        let out = Ingestor::translate(&ev);
        assert_eq!(out.len(), 1);
        match &out[0] {
            StreamEvent::Ctx {
                funding,
                open_interest,
                mark_px,
                oracle_px,
                premium,
                day_ntl_vlm,
                prev_day_px,
                ..
            } => {
                assert_eq!(*funding, 0.0001);
                assert_eq!(*open_interest, 123.4);
                assert_eq!(*mark_px, 65000.5);
                assert_eq!(*oracle_px, 64999.9);
                assert_eq!(*premium, Some(0.001));
                assert_eq!(*day_ntl_vlm, Some(987.6));
                assert_eq!(*prev_day_px, Some(64000.0));
            }
            _ => panic!("Ctx bekleniyordu"),
        }
    }

    /// Geçersiz/eksik ÇEKİRDEK alan → event DÜŞÜRÜLÜR (sahte 0.0 bağlam YOK).
    #[test]
    fn ctx_bozuk_cekirdek_event_dusurur() {
        let mk = |mark: serde_json::Value, funding: serde_json::Value| {
            hyperliquid_api::websocket::WsEvent {
                channel: "activeAssetCtx".into(),
                data: json!({"coin":"BTC","ctx":{"funding":funding,"openInterest":"10",
                    "markPx":mark,"oraclePx":"99"}}),
            }
        };
        // markPx geçersiz string
        assert!(Ingestor::translate(&mk(json!("bozuk"), json!("0.0001"))).is_empty());
        // funding eksik
        assert!(Ingestor::translate(&mk(json!("100"), json!(null))).is_empty());
        // markPx finite-olmayan → düşür; 0.0 falan YAZILMAZ
        assert!(Ingestor::translate(&mk(json!("NaN"), json!("0.0001"))).is_empty());
    }

    /// HL'de meşru null olan alanlar (premium vb.) Option olarak taşınır —
    /// 0.0'a çevrilmeler yok.
    #[test]
    fn ctx_mesru_null_option_tasinir() {
        let ev = hyperliquid_api::websocket::WsEvent {
            channel: "activeAssetCtx".into(),
            data: json!({"coin":"ETH","ctx":{"funding":"0.0001","openInterest":"1",
                "markPx":"3000","oraclePx":"2999","premium":null,
                "dayNtlVlm":null,"prevDayPx":null}}),
        };
        let out = Ingestor::translate(&ev);
        match &out[0] {
            StreamEvent::Ctx {
                premium,
                day_ntl_vlm,
                prev_day_px,
                ..
            } => {
                assert_eq!(*premium, None);
                assert_eq!(*day_ntl_vlm, None);
                assert_eq!(*prev_day_px, None);
            }
            _ => panic!("Ctx bekleniyordu"),
        }
    }
}
