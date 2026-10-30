//! DATA — piyasa verisi hattı: WS bağlantı → tick → mum + bağlam.
//!
//! Akış topolojisi:
//! `Ingestor` (shard'lı WS) → `StreamEvent` → `Aggregator` (tick→mum,
//! taker akışı) / `MarketCtx` (funding/OI/mark serileri) → motor.
//! Kalıcılık `Storage` arkasında: ClickHouse (tarihsel arşiv, yalnız
//! backfill_job), Memory (test/fallback). Evren `TradingUniverse` tek
//! kaynaktır (engine/ingestor/risk/WS aynı listeden).
pub mod aggregator;
pub mod backfill_job;
pub mod ingestor;
pub mod market_ctx;
pub mod storage;
pub mod universe;

pub use aggregator::{Aggregator, AggregatorConfig, Candle};
pub use ingestor::{CoinInfo, Ingestor, IngestorConfig, StreamEvent, Universe};
pub use market_ctx::{CtxState, MarketCtx};
pub use storage::{MemoryStorage, RedisCache, Storage, TradeRecord};
pub use universe::{TradingUniverse, UniverseSpec};
