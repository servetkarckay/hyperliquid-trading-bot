//! # data
//!
//! Veri katmanı: evren geneli (tüm aktif coinler) canlı veri akışı,
//! tick->candle dönüşümü, piyasa bağlamı serileri ve kalıcı depolama.
//!
//! VERİ AKIŞI (üretim topolojisi):
//!   Hyperliquid WS (shard'lı bağlantılar)
//!     -> Ingestor   : trades + allMids + activeAssetCtx + l2Book olayları
//!     -> Aggregator : tick -> çoklu interval candle (1m..1d)
//!     -> MarketCtx  : canlı funding/OI/mark serileri (ΔOI, basis)
//!     -> Storage    : Redis (SICAK önbellek) + ClickHouse (KALICI arşiv)
//!
//! Depolama rolleri:
//! - Redis  : SON durumun milisaniyelik erişimi (mids haritası, coin başına
//!            son ctx). Strateji sorguları API'ye gitmeden cache'ten okur;
//!            TTL ile bayat veri otomatik düşer. REDIS_URL ile aktif.
//! - ClickHouse: yüksek hacimli tick/candle/ctx zaman serilerinin kalıcı
//!            arşivi; SQL ile geçmiş analizi, backtest verisi, ΔOI/funding
//!            serileri. CLICKHOUSE_URL ile aktif.
//! İkisi de opsiyoneldir: URL boşsa sistem Memory ile çalışır.
pub mod aggregator;
pub mod backfill_job;
pub mod ingestor;
pub mod market_ctx;
pub mod storage;
pub mod universe;

pub use aggregator::{Aggregator, AggregatorConfig, Candle};
pub use ingestor::{CoinInfo, Ingestor, IngestorConfig, StreamEvent, Universe};
pub use universe::{TradingUniverse, UniverseSpec};
pub use market_ctx::{CtxState, MarketCtx};
pub use storage::{MemoryStorage, RedisCache, Storage, TradeRecord};
