//! # strategy
//!
//! Sinyal üretim katmanı: 19 canlı strateji + kayıt defteri (registry).
//!
//! MİMARİ KURAL: stratejiler pozisyon AÇMAZ, SİNYAL üretir. Her strateji
//! `MarketInput` snapshot'ını alır, `Signal` döndürür:
//!   Long/Short { strength: -1..1 } — yön + kayıtsız güç
//!   Hold { note } — sinyal yok, karar gerekçesi notta
//!   Exit { note } — pozisyon kapatma/değişim önerisi
//!   Skip { reason } — veri yetersizliği (SAHTE sinyal üretmek yasak)
//! Emir dönüştürme Executor + RiskManager kapısından geçer; burada yok.
//!
//! VERİ AKIŞI: snapshot şunları taşır — 1h mumlar (kronolojik), canlı bağlam
//! (funding/OI/mark: data::MarketCtx), OI değişimi, taker buy oranı, defter
//! dengesi + en iyi bid/ask, evren geneli 1g getiri sıralaması, opsiyonel
//! harici IV, equity, mevcut envanter ve parent emir boyutu.
//!
//! `registry()`: tüm stratejileri döner; 19 strateji + 1 meta (sistem
//! testi) + 1 proxy (değer yatırım).
pub mod wyckoff;
pub mod kesit_momentum;
pub mod lob_emir_akisi;
pub mod market_making;
pub mod optimal_execution;
pub mod volatilite_zaman_serisi;
pub mod carry_funding_basis;
pub mod oi_likidasyon;
pub mod rejim_tespiti;
pub mod risk_kelly;
pub mod dow_teorisi;
pub mod davranissal_finans;
pub mod ensemble;
pub mod klasik_indikatorler;
pub mod elliott_dalga;
pub mod deger_yatirim;
pub mod trend_following_turtle;
pub mod breakout_kirilim;
pub mod volatilite_arbitraj;

use data::market_ctx::CtxState;
use data::Candle;

/// Strateji girdisi: tüm hesaplamalar bu snapshot üzerinden yapılır.
pub struct MarketInput {
    pub coin: String,
    /// 1h mumlar, kronolojik (eskiden yeniye)
    pub candles: Vec<Candle>,
    /// canlı bağlam (funding/OI/mark) — data::MarketCtx'ten
    pub ctx: Option<CtxState>,
    /// OI değişimi (son iki ölçüm)
    pub oi_delta: Option<f64>,
    /// taker buy oranı (0..1)
    pub taker_ratio: Option<f64>,
    /// üst N seviye defter dengesi [-1,1]
    pub book_imbalance: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    /// evren geneli 1g getiriler: `mark_px / prev_day_px − 1` (`engine_loop.rs`,
    /// `activeAssetCtx`) — s03 kesit sıralaması ve `crisis_mode`. **7g değil.**
    pub universe_returns: Vec<(String, f64)>,
    /// harici IV (Deribit) — s24 opsiyonel yol
    pub iv: Option<f64>,
    /// hesap equity (sizing)
    pub equity: f64,
    /// mevcut envanter (USD, +long / -short) — s05/s06
    pub inventory_usd: f64,
    /// yürütülecek parent emir büyüklüğü (USD) — s06
    pub order_size_usd: f64,
}

impl Default for MarketInput {
    fn default() -> Self {
        Self {
            coin: String::new(),
            candles: vec![],
            ctx: None,
            oi_delta: None,
            taker_ratio: None,
            book_imbalance: None,
            best_bid: None,
            best_ask: None,
            universe_returns: vec![],
            iv: None,
            equity: 0.0,
            inventory_usd: 0.0,
            order_size_usd: 0.0,
        }
    }
}

/// Strateji çıktısı. strength ∈ [-1,1] (kayıt dışı güç, sizing risk katmanında).
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    Long { strength: f64, note: String },
    Short { strength: f64, note: String },
    Hold { note: String },
    /// Kapanış/değişim önerisi (örn. sıralama kayması, kapanan kırılım)
    Exit { note: String },
    /// Harici veri / yeterli veri yok — SAHTE sinyal üretilmez
    Skip { reason: String },
}

pub trait Strategy: Send + Sync {
    fn name(&self) -> &'static str;
    /// Hangi dokümanı uyguladığı
    fn evaluate(&self, input: &MarketInput) -> Signal;
}

/// 00-strateji-katalogu.md — tüm stratejilerin kayıt defteri
pub fn registry() -> Vec<Box<dyn Strategy>> {
    vec![
        Box::new(wyckoff::Wyckoff),
        Box::new(kesit_momentum::KesitMomentum),
        Box::new(lob_emir_akisi::LobEmirAkisi),
        Box::new(market_making::MarketMaking),
        Box::new(optimal_execution::OptimalExecution),
        Box::new(volatilite_zaman_serisi::VolatiliteZamanSerisi),
        Box::new(carry_funding_basis::CarryFundingBasis),
        Box::new(oi_likidasyon::OiLikidasyon),
        Box::new(rejim_tespiti::RejimTespiti),
        Box::new(risk_kelly::RiskKelly),
        Box::new(dow_teorisi::DowTeorisi),
        
        Box::new(davranissal_finans::DavranissalFinans),
        Box::new(klasik_indikatorler::KlasikIndikatorler),
        Box::new(elliott_dalga::ElliottDalga),
        Box::new(deger_yatirim::DegerYatirim),
        Box::new(trend_following_turtle::TurtleTrendFollowing),
        Box::new(breakout_kirilim::BreakoutKirilim),
        Box::new(volatilite_arbitraj::VolatiliteArbitraj),
        
    ]
}

/// Yaygın yardımcılar
pub(crate) fn closes(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.c).collect()
}
pub(crate) fn highs(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.h).collect()
}
pub(crate) fn lows(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.l).collect()
}
pub(crate) fn volumes(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.v).collect()
}
/// Son değer (pencere doluysa)
pub(crate) fn last(v: &[Option<f64>]) -> Option<f64> {
    v.last().and_then(|x| *x)
}

/// Registry'deki strateji sayısı (nötr cold-start logu + testler).
pub const STRATEGY_COUNT: usize = 18;
