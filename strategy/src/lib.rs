//! STRATEGY — sinyal sözleşmesi ve 15-strateji registry.
//!
//! Sözleşme:
//! 1. `Strategy::evaluate(&MarketInput) -> Signal` — SAFTIR: pozisyon
//!
//! açamaz, ağ erişemez; panik motorca izole edilir (Skip).
//! 2. `Signal`: Long/Short { strength ∈ [0,1] } / Hold / Exit (kapanış
//!
//! önerisi — engine exit kanalına gider) / Skip (veri yok — SAHTE
//! sinyal üretilmez; `unwrap_or(0.5)` fallback'leri yasak).
//! 3. `Strategy::status(input)`: ACTIVE / AUXILIARY (yönsüz yardımcı:
//!
//! Kelly/MM) — HL'de girdisi yapısal olarak olmayan strateji
//! (IV feed, temel veri, parent-order) registry'de YER ALMAZ;
//! sessiz ölü strateji raporlanmak zorundadır.
//! 4. `MarketInput`: kapanmış mumlar + canlı son bar + canlı bağlam
//!
//! (funding/OI/taker/imbalance/universe_returns).
//! 5. `registry`: tek temel modül (MRE); `test_candles` yalnız-test
//!
//! deterministik mum üreteci.
pub mod ensemble;
pub mod mre;
pub mod regime_detection;

use data::market_ctx::CtxState;
use data::Candle;

/// Strateji girdisi: tüm hesaplamalar bu snapshot üzerinden yapılır.
pub struct MarketInput {
    pub coin: String,
    /// 1h mumlar, kronolojik sıra (eski→yeni)
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
    /// `activeAssetCtx`) — kesit sıralaması ve `crisis_mode`. **7g değil.**
    pub universe_returns: Vec<(String, f64)>,
    /// harici IV (Deribit) — opsiyonel yol
    pub iv: Option<f64>,
    /// hesap equity (sizing)
    pub equity: f64,
    /// mevcut envanter (USD, +long / -short) — /
    pub inventory_usd: f64,
    /// yürütülecek parent emir büyüklüğü (USD) —
    pub order_size_usd: f64,
    /// **PORTFÖY win-rate** — tracker'ın TOPLAM canlı kanıtından (engine
    /// doldurur). `None` = kanıt yok. Kelly sizing bunun dışındaki hiçbir
    /// kaynaktan win-rate ALMAZ (sabit %45 yasağı).
    pub portfolio_win_rate: Option<f64>,
    /// **PORTFÖY k/z oranı** (ortalama kâr R / ortalama zarar |R|) — `None` = kanıt yok.
    pub portfolio_wl_ratio: Option<f64>,
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
            portfolio_win_rate: None,
            portfolio_wl_ratio: None,
        }
    }
}

/// Strateji çıktısı. strength ∈ [-1,1] (kayıt dışı güç, sizing risk katmanında).
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    Long {
        strength: f64,
        note: String,
    },
    Short {
        strength: f64,
        note: String,
    },
    Hold {
        note: String,
    },
    /// Kapanış/değişim önerisi (örn. sıralama kayması, kapanan kırılım)
    Exit {
        note: String,
    },
    /// Harici veri / yeterli veri yok — SAHTE sinyal üretilmez
    Skip {
        reason: String,
    },
}

pub trait Strategy: Send + Sync {
    fn name(&self) -> &'static str;
    /// Hangi dokümanı uyguladığı
    fn evaluate(&self, input: &MarketInput) -> Signal;

    /// **ÇIKIŞ NİYETİ** — pozisyon-farkında çıkış kuralı.
    ///
    /// "Strateji kuralları TEK YERDE" prensibi: çıkış kuralı stratejinin
    /// KENDİSİNDE yaşar (durum dahil — önceki döngü bandı gibi); motor
    /// pozisyon state'ini verir ve niyeti YÖNLENDİRİR (`ExitNow`), kuralı
    /// BİLMEZ. Varsayılan: yok (çoğu strateji çıkış kuralı tanımlamaz).
    ///
    /// `ctx.position = None` (Flat) → niyet sorulmaz → emir üretilmez.
    fn exit_intent(&self, ctx: &ExitCtx) -> Option<ExitIntent> {
        let _ = ctx;
        None
    }

    /// Sistem raporu için durum (sistem raporu `ACTIVE / AUXILIARY /
    /// DATA-MISSING` ayrımı yapabilmeli).
    ///
    /// Varsayılan: girdisi tam gelen yönlü strateji (`ACTIVE`).
    /// Geçersiz kılanlar (kendisini en iyi bilen stratejinin kendisidir):
    /// `Kelly Sizing`, `Market Making` → `AUXILIARY`
    /// (yön üretmez, gerçek parayla giriş sinyali DEĞİL).
    fn status(&self, input: &MarketInput) -> StratStatus {
        let _ = input;
        StratStatus::Active
    }
}

/// Stratejinin bu döngüdeki **rapor durumu** (sessiz ölü strateji yasağı):
///
/// | Durum | Anlamı |
/// |-------|--------|
/// | `Active` | girdisi tam, yönlü sinyal üretebilir |
/// | `Auxiliary` | giriş stratejisi DEĞİL (boyut/kote/plan önerisi) |
/// | `DataMissing` | girdisi bağlı değil → `Skip` (SAHTE veri üretmez) |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StratStatus {
    Active,
    Auxiliary,
    DataMissing,
}

impl StratStatus {
    pub fn label(self) -> &'static str {
        match self {
            StratStatus::Active => "ACTIVE",
            StratStatus::Auxiliary => "AUXILIARY",
            StratStatus::DataMissing => "DATA-MISSING",
        }
    }
}

/// 00-strateji-katalogu.md — tüm stratejilerin kayıt defteri
/// Çıkış-niyeti çağrısı için motor bağlamı: stratejiye pozisyon state'ini
/// ve kendi kuralının girdilerini tek yerde verir.
#[derive(Debug, Clone)]
pub struct ExitCtx {
    pub coin: String,
    /// kapanmış mum kuyruğu (kanal-tarzı kurallar için — son ~200 bar)
    pub candles: Vec<Candle>,
    /// evren geneli 1g getiriler (kesit sıralaması için)
    pub universe_returns: Vec<(String, f64)>,
    /// AÇIK pozisyon (engine state); `None` = Flat → niyet sorulmaz
    pub position: Option<ExitPositionInfo>,
}

/// Açık pozisyonun taraf bilgisi (engine'den stratejiye)
#[derive(Debug, Clone, Copy)]
pub struct ExitPositionInfo {
    pub is_long: bool,
    pub size: f64,
    pub entry_px: f64,
}

/// Stratejinin tetiklenen çıkış kuralı (log/audit notu taşır)
#[derive(Debug, Clone, PartialEq)]
pub struct ExitIntent {
    pub note: String,
}

/// Registry stratejilerinin adları (test/rapor için; `registry()`nin
/// ağırlıksız karşılığı).
pub fn registry_names() -> Vec<&'static str> {
    vec!["MRE"]
}

pub fn registry() -> Vec<Box<dyn Strategy>> {
    vec![Box::new(mre::Mre)]
}

/// **BİLGİ KÜMESİ** — stratejilerin aynı temel girdiden türetilmesi
/// ("18 bağımsız alpha" İLLÜZYONU): fiyat/trend/volume aynı kaynak olan
/// stratejiler aynı anda aynı yöne oy verdiğinde bu 6 bağımsız kanıt
/// DEĞİL, aynı faktörün 6 formülasyonudur. Tahsis genişlik ödülü bu
/// yüzden küme sayısına göre verir (aynı küme = çift sayılmaz).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InfoCluster {
    /// fiyat/trend/volume türevleri: Wyckoff, Dow, Turtle,
    /// Squeeze Breakout, Elliott, Classic Indicators, Regime Detection
    Trend,
    /// kontra/ortalama-dönüş: Davranışsal
    MeanReversion,
    /// defter/akış: Order Flow Imbalance, Market Making
    OrderFlow,
    /// evren geneli göreli güç: Cross-Sectional Momentum
    CrossSectional,
    /// funding/basis taşıma: Carry
    Carry,
    /// vol modelleri: GARCH (koşullu volatilite rejimi)
    Volatility,
    /// OI/likidasyon kadransı
    Liquidation,
    /// boyut önerisi (yön yok): Kelly
    Sizing,
}

/// strateji adı → bilgi kümesi (registry ile birebir; tek kaynak)
pub fn info_cluster(name: &str) -> InfoCluster {
    match name {
        // MRE: tepki/volatilite kesit kompoziti → MeanReversion
        "MRE" => InfoCluster::MeanReversion,
        // bilinmeyen ad → fiyat/trend türevi varsayımı (küme mekaniği
        // testleri keyfi isimlerle ayrışmayı test eder)
        _ => InfoCluster::Trend,
    }
}

/// Yaygın yardımcılar
pub(crate) fn closes(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.c).collect()
}

/// Registry'deki strateji sayısı (nötr cold-start logu + testler).
pub const STRATEGY_COUNT: usize = 1;

#[cfg(test)]
pub mod test_candles;
