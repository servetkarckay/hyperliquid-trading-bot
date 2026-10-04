//! MOTOR DÖNGÜSÜ — veri → strateji → ensemble → TAHSİS → durum makinesi →
//! toplu emir.
//!
//! İKİ FAZ (Portföy optimizasyonu mimarisi):
//!   FAZ 1 — TARAMA: her coin → stratejiler → ensemble kararı (skor, oylar)
//!   FAZ 2 — TAHSİS: allocation_manager — inanç skoru = (oy yoğunluğu ×
//!           dinamik-Kelly ağırlığı × güc) / ATR marjı;
//!           korelasyon >%70 coinler ayrı seçilmez (çeşitlendirme zorunluluğu);
//!           conviction-temelli sermaye dilimleri (top-N)
//!   FAZ 3 — YÜRÜTME: seçilen coinlerin gelmiş emirleri + tüm coinlerin
//!           exit/cancel eylemleri — BİR imzalı POST (batching)
//!
//! ATTPIBUTION: giriş atan stratejiler kaydedilir; pozisyon kapanınca
//! tracker'a (win/loss) işlenir → son 100 işlem penceresinden kelly f*
//! yenilenir; win-rate <%35 → ağırlık sıfır (sistem adaptif).
//! Bütçe: equity × f*; tahsis dilimi en yüksek inanç önce.

use std::collections::HashMap;

use hyperliquid_api::exchange_endpoint::WireOrder;
use hyperliquid_api::{Config, Info, Tif};
use strategy::ensemble::{ConsensusSignal, Dir, Ensemble, EnsembleConfig, RegimeGate};
use strategy::rejim_tespiti::{classify};
use strategy::{MarketInput, Signal, Strategy};

use crate::allocation_manager::{AllocationConfig, AllocationManager, Candidate, Vote};
use crate::executor::Executor;
use crate::state_machine::{
    Action, Decision, Machines, PosState, PositionStateMachine, SmConfig, Side as SmSide,
};
use crate::track::{StrategyTracker, tid_seen, write_trade_exit, TradeExit};

#[derive(Debug, Clone)]
pub struct LoopConfig {
    pub coins: Vec<String>,
    pub interval_secs: u64,
    pub hist_bars: usize,
    pub min_score: f64,
    pub batch_window_ms: u64,
    /// tahsis motoru: kaç coine girilir
    pub top_n: usize,
    pub max_position_usd: f64,
    /// Trailing stop açık mı (EXCHANGE tarafında ratchet).
    /// Kapatmak için `TRAIL_ON=0`. Kapatıldığında davranış §8.3.15'teki
    /// sabit-stop haline döner.
    pub trail_on: bool,
    /// Stop mesafesi = k × ATR. 1.5 varsayılan (giriş stop'u ile aynı).
    pub trail_atr_k: f64,
    /// Trailing'in tetiklenmesi için stop'un en az bu kadar iyileşmesi gerekir
    /// (ATR'nin kesri). 0.25×ATR altındaki iyileştirmeye emir yaktırmaya değmez:
    /// her tetikleme 1 `place_trigger` + 1 `cancel` = 2 imzalı POST.
    pub trail_min_step: f64,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            coins: vec!["BTC".into()],
            interval_secs: 3600,
            hist_bars: 300,
            min_score: 0.30,
                batch_window_ms: 100,
            top_n: 3,
            max_position_usd: 50.0,
            trail_on: true,
            trail_atr_k: 1.5,
            trail_min_step: 0.25,
        }
    }
}

/// Aynı döngüde hesaplanmış bir coin'in değerlendirmesi
struct CycleEval {
    coin: String,
    cs: ConsensusSignal,
    bid: f64,
    ask: f64,
    atr_pct: f64,
    /// son 200 kapanış (korelasyon hesabı için)
    closes: Vec<f64>,
}

/// Pozisyon başına trailing stop durumu + MFE/MAE (excursion) kaydı.
///
/// TRAİLİNG STOP KARARI (gerekçeli):
/// - **SL ratchet'lenir.** Tepe (peak) lehli yönde ilerledikçe stop
///   `tepe ∓ k×ATR`'ye çekilir; **asla** gevşemez.
/// - **TP SABİT KALIR** (3×ATR). Trailing TP, 3:1'lik hedefi başarısız bir
///   yaklaşımda ~0.8:1'e çevirip işlemi kârsız kapattırır — yani kâr elde
///   etme olasılığını düşürür. Trailing yalnız **zararı sınırlar**.
///
/// `peak`/`worst` yalnız mark fiyatıyla güncellenir (bar yüksek/almak yerine):
/// 1 saatlik bar'da tepe, stop'u borsada olduğu anda tetikleyecek bir seviyeye
/// taşıyabilir; mark daha gerçekçi ve daha muhafazakâr.
#[derive(Debug, Clone)]
pub struct TrailState {
    pub is_long: bool,
    pub entry_px: f64,
    /// Mutlak ATR (fiyat birimi). Stop mesafesi = k × bu.
    pub atr_px: f64,
    /// En lehli görülen fiyat (long: max, short: min)
    pub peak_px: f64,
    /// En aleyhli görülen fiyat (long: min, short: max)
    pub worst_px: f64,
    pub last_px: f64,
    /// Borsada o an duran stop'un fiyatı (gevşememe karşılaştırması için)
    pub cur_sl_px: f64,
    pub opened_ms: u64,
    /// kaç kez stop seviyesi yükseltildi
    pub ratchets: u32,
}

/// Giriş atribüsyonu: (katkı veren stratejiler, pozisyon yönü, giriş fiyatı, çıkış sebebi)
pub struct EntryAttr {
    pub coin: String,
    pub voters: Vec<String>,
    pub side: SmSide,
    pub entry_px: f64,
    /// giriş riski (|entry - SL|); R-çarpanı = net PnL / risk
    pub risk_px: f64,
    /// giriş boyutu — risk_usd = risk_px × size
    pub size: f64,
    pub ts: u64,
    /// **ANALYTICAL TP HEDEFİ** — gerçek dolum (`entry_px`) üzerinden
    /// `AllocationConfig::tp_from_entry()` ile türetilir.
    ///
    /// ⚠️ `TakeProfit` **bugün gerçek bir kapanış kaynağı değil** (analytical
    /// target); TP-execution yolu bağlanınca `exit_reason=TakeProfit` atanacaktır.
    /// Borsada TP limit emri asılmaz; bu değer yalnız MFE/R analizi ve
    /// "TP'ye ulaşsaydı kârlı mıydı" ölçümü içindir.
    pub tp_px: Option<f64>,
    /// çıkış sebebi: gerçek kapanış sebebi (MFE>0 TP demek DEĞİL)
    pub exit_reason: Option<ExitReason>,
}

impl EntryAttr {
    /// Analytical TP'yi **gerçek dolumdan** türetip kaydeder (P1-E).
    ///
    /// `entry_px` = gerçek dolum (REST `entryPx`), `risk_px` = gerçek SL
    /// mesafesi (stop entry'ye göre taşınır → kayma risk mesafesini değiştirmez).
    /// Geçersiz giriş/risk'te `None` döner ve eski hedef **silinmez**
    /// (yarım kalan veri, sıfırlanmış veriden daha dürüst).
    pub fn set_analytical_tp(&mut self, cfg: &AllocationConfig) -> Option<f64> {
        let dir = if self.side.is_buy() { Dir::Long } else { Dir::Short };
        let tp = cfg.tp_from_entry(dir, self.entry_px, self.risk_px);
        if !(tp > 0.0) || !tp.is_finite() {
            return None;
        }
        self.tp_px = Some(tp);
        Some(tp)
    }

    /// REST `entryPx` (gerçek dolum) geldiğinde `entry_px`'i düzeltir ve
    /// analytical TP'yi **yeniden** türetir. `true` = kayma ölçüldü ve düzeltildi.
    ///
    /// Kayma ölçülür (`[SL] … kayma=%`) çünkü aynı değer stop fiyatını ve
    /// R-çarpanını da besler. TP yeniden türetilmezse yanlış fiyatta kalır:
    /// long'da dolum yukarı kaydıysa TP eski fiyatta (kâr tarafında ama
    /// gerekenden yakın), short'ta aşağı kaydıysa TP **zarar tarafına** düşer.
    pub fn apply_real_fill(&mut self, rest_entry: f64, cfg: &AllocationConfig) -> bool {
        if !(rest_entry > 0.0) || !rest_entry.is_finite() {
            return false;
        }
        let gap = (rest_entry - self.entry_px) / self.entry_px.abs().max(1e-12);
        if gap.abs() <= 1e-9 {
            return false;
        }
        let planned = self.entry_px;
        self.entry_px = rest_entry;
        let tp = self.set_analytical_tp(cfg);
        tracing::info!(
            "[SL] {} giriş fiyatı düzeltildi: karar(defter)={:.8} dolum(REST)={:.8} \
             kayma={:+.3}% → analytical TP={}",
            self.coin,
            planned,
            rest_entry,
            gap * 100.0,
            tp.map(|t| format!("{t:.8}")).unwrap_or_else(|| "-".into())
        );
        true
    }
}

/// Gerçek çıkış sebebi — MFE/MAE analizi için critical
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Default)]
pub enum ExitReason {
    #[default]
    Unknown,
    StopLoss,          // borsa SL tetiklendi
    TakeProfit,        // TP limit order doldu
    TrailingStop,      // trailing ratchet tetiklendi
    SignalExit,        // ensemble sinyali ters (ExitNow)
    EmergencyFlatten,  // kill-switch / black-swan flatten
    Manual,            // manuel müdahale
}

pub struct Engine {
    pub loop_cfg: LoopConfig,
    pub executor: Executor,
    pub info: Info,
    agg: data::Aggregator,
    ctx: data::market_ctx::MarketCtx,
    ensemble: Ensemble,
    strategies: Vec<Box<dyn Strategy>>,
    machines: Machines,
    books: HashMap<String, (f64, f64, f64)>,
    tracker: StrategyTracker,
    /// **Kaydedilmiş canlı ağırlık snapshot'ı** (`origin:"live"` dosyası).
    ///
    /// AYRI kavramlar (bilerek karıştırılmıyor):
    /// - `live_weights` — *ağırlıkların kendisi* (strateji → sayı). `bot.rs`
    ///   `ensemble_weights_live.json`'dan doldurur; **gerçek ensemble'a girer**.
    /// - `tracker` — *canlı işlem kanıtı* (n, ΣR, win rate). Ağırlık YOK,
    ///   yalnız ağırlığın nasıl türetileceğini söyleyen kanıt.
    ///
    /// Önceki `trained_weights: HashMap<String, TrainedInfo>` **hiç okunmuyordu**
    /// (ölü state) ve `apply_effective_weights()` onu sessizce eziyordu.
    live_weights: HashMap<String, f64>,
    /// öğrenme politikası: n eşikleri (ENV `LEARN_MIN_N` / `LEARN_FULL_N`)
    pub learn_pol: crate::track::LearnPolicy,
    /// registry'deki strateji adları (nötr cold-start listesi)
    strategy_names: Vec<String>,
    /// tracker durumu dosyası (ENV `TRACK_STATE_PATH`)
    pub track_state_path: String,
    /// keşif şeridi açık mı (ENGINE_EXPLORE)
    pub explore_on: bool,
    /// keşif hakkı olan stratejiler (`n < full_n`) — veri açlığı kuyruğu
    explore_names: std::collections::HashSet<String>,
    /// keşif round-robin imleci (her döngüde bir tur ilerler)
    explore_cursor: usize,
    /// BU DÖNGÜDE sırası gelen keşifçi (P0-C: en fazla **bir**).
    /// `explore_names` = kuyruk (veri açlığı olanlar), bu = sıradaki.
    explore_pick: Option<String>,
    /// **ExitsOnly** kipi (P1-F) ve risk kilidi tek kapıda: `gate`.
    /// `entries_allowed` ALAN DEĞİL, `self.entries_allowed()` ile türetilir.
    /// Alan kaldırıldığı için hiçbir runtime yolu onu doğrudan yazamaz.
    gate: EntryGate,
    /// HESAP kaldıracı (cross) — açılışta borsaya yazılır, risk kontrolüne girer
    pub leverage: f64,
    entry_attr: HashMap<String, EntryAttr>,
    /// coin -> trailing stop durumu (§8.3.15)
    trail: HashMap<String, TrailState>,
    /// coin -> mutlak ATR (fiyat birimi). Tarama fazında hesaplanır, trailing
    /// fazında okunur (`sync_exchange_sl` `evals`'e erişemez).
    atr_px: HashMap<String, f64>,
    am: AllocationManager,
    risk: crate::RiskManager,
    n_cycles: u64,
    funding_ema: HashMap<String, f64>,
    funding_freeze_until: HashMap<String, u64>,
    oid_counter: i64,
    pub equity: f64,
    /// Veri toplama dönemi (A1): her deploy/parametre değişikliğinde yeni epoch.
    epoch: String,
    /// Git SHA (kısaltılmış) — trade_exits.jsonl'ye yazılır.
    git_sha: String,
    /// trade_exits.jsonl dosya yolu (ENV `TRADE_EXITS_PATH`, default: config/trade_exits.jsonl)
    trade_exits_path: String,
    /// P1-3: Son recon zamanı (mutex yerine engine field)
    last_recon_ms: u64,
    /// P0-A: Risk lock — black-swan/flatten-fail sonrası kalıcı kilit.
    /// Sadece black-swan/flatten-fail yolu true yapar.
    /// health_ok && !risk_locked = entries_allowed.
    /// Açılma: sync_positions_from 2 ardışık döngü flat doğrularsa otomatik.
    ///
    /// P1: bu döngüde pozisyon haritası `sync_positions_from` ile tazelendi mi?
    /// Auto-unlock flat doğrulaması YALNIZ bu doğrulanmış düzlemlerde ilerler.
    rest_sync_verified: bool,
}

/// **Canlı ağırlık dosyasını doğrula ve sayısal ağırlıklara çevir** (saf fonksiyon).
///
/// Üretimde `Engine::load_live_weights` yalnızca bunu çağırır + saklar;
/// testler de doğrudan BUNU sınar (kopyalanmış taklit yok).
///
/// Doğrulama kuralları (sessiz düşüş YOK — `Err` döner):
/// 1. `origin == "live"` — backtest çıkışı look-ahead biaslı, reddedilir.
/// 2. `weights` bir JSON **nesnesi** olmalı.
/// 3. Her değer bir JSON **sayı** olmalı.
/// 4. Değer `finite` ve `>= 0.0` olmalı.
///
/// **Üst sınır UYGULANMAZ**: `WEIGHT_CLAMP_HI = 2.0` yalnız tracker kanıtından
/// türetilen ağırlıkları sınırlar (`StrategyTracker::effective_weights`).
/// Kaydedilmiş canlı değerler **birebir** korunur: dosyada `1.75` varsa
/// ensemble'a `1.75` girer.
pub fn parse_live_weights(raw: &serde_json::Value) -> anyhow::Result<HashMap<String, f64>> {
    match raw.get("origin").and_then(|x| x.as_str()) {
        Some("live") => {}
        Some(other) => anyhow::bail!(
            "canlı ağırlık dosyası origin=\"{other}\" — yalnız \"live\" kabul edilir \
             (backtest çıkışı look-ahead biaslı)"
        ),
        None => anyhow::bail!(
            "canlı ağırlık dosyasında \"origin\" alanı yok — \"live\" doğrulanamaz"
        ),
    }
    let obj = raw
        .get("weights")
        .and_then(|x| x.as_object())
        .ok_or_else(|| {
            anyhow::anyhow!("canlı ağırlık dosyasında \"weights\" nesnesi yok — geçersiz şema")
        })?;
    let mut m: HashMap<String, f64> = HashMap::with_capacity(obj.len());
    for (name, v) in obj {
        let w = v.as_f64().ok_or_else(|| {
            anyhow::anyhow!(
                "canlı ağırlık \"{name}\" sayı DEĞİL — beklenen canlı şeması: \
                 origin=live + weights={{ \"Wyckoff\": 0.83 }}"
            )
        })?;
        if !w.is_finite() {
            anyhow::bail!("canlı ağırlık \"{name}\" sonlu değil ({w})");
        }
        if w < 0.0 {
            anyhow::bail!(
                "canlı ağırlık \"{name}\" negatif ({w}) — kâr maksimizasyonu negatif ağırlık koymaz"
            );
        }
        m.insert(name.clone(), w);
    }
    Ok(m)
}

/// `[RISK-LOCK] AKTİF` logunun kalan pozisyon parçası — **saf** bicimlendirici.
///
/// Çıktı tam olarak `[(coin,sz,direction),(coin,sz,direction)]` şeklindedir
/// (`direction` = boyut işareti: `>0` long, `<0` short). Testler bu biçimi
/// doğrulayarak log sözleşmesini sabitler.
pub fn risk_lock_positions_left(left: &[(String, f64)]) -> String {
    if left.is_empty() {
        return "[]".to_string();
    }
    format!(
        "[{}]",
        left.iter()
            .map(|(c, sz)| format!("({c},{sz},{})", if *sz > 0.0 { "long" } else { "short" }))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Canlı dosyada **OLMAYAN** stratejiler (sessiz "nötr" yok — açıkça raporlanır).
///
/// Üretimde `Engine::load_live_weights` ve testler aynı fonksiyonu çağırır.
pub fn missing_live_strategies(
    strategy_names: &[String],
    live: &HashMap<String, f64>,
) -> Vec<String> {
    strategy_names
        .iter()
        .filter(|n| !live.contains_key(*n))
        .cloned()
        .collect()
}

/// `merge_effective_weights` çıktısı (saf) — hangi ağırlık nereden geldi?
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveWeightsMerge {
    /// **Ensemble'a basılacak nihai ağırlıklar.**
    pub ensemble_weights: HashMap<String, f64>,
    /// Canlı snapshot'tan **korunan** (tracker kanıtı olmayan) stratejiler.
    pub live_applied: Vec<String>,
    /// Canlı snapshot'ta olup tracker kanıtı **üstüne yazan** stratejiler.
    pub live_overridden: Vec<String>,
    /// Dosyada var ama registry'de **yok** → ensemble'a girmeyenler.
    pub unknown: Vec<String>,
}

/// **İki kaynağı birleştir** (saf fonksiyon — üretimde
/// `Engine::apply_effective_weights` yalnızca bunu çağırır).
///
/// Öncelik:
/// 1. tracker kanıtı varsa (`n >= min_n`) → tracker'ın türettiği ağırlık KAZANIR
///    (yeni gerçek kanıt snapshot'ın üstüne yazar).
/// 2. tracker kanıtı yoksa → canlı snapshot **KORUNUR**; o da yoksa 1.0
///    (tracker tarafının cold-start değeri `tracker_w`'de zaten 1.0'dır).
///
/// Kural 2, "canlı dosya okunuyor ama bir sonraki uygulamada sessizce 1.0'a
/// eziliyor" hatasını (asıl BLOCKER) kapatır.
pub fn merge_effective_weights(
    tracker_w: &HashMap<String, f64>,
    live: &HashMap<String, f64>,
    evidence_n: &dyn Fn(&str) -> usize,
    min_n: usize,
) -> LiveWeightsMerge {
    let mut out = LiveWeightsMerge {
        ensemble_weights: tracker_w.clone(),
        ..Default::default()
    };
    for (name, lw) in live {
        if !tracker_w.contains_key(name) {
            out.unknown.push(name.clone());
            continue;
        }
        if evidence_n(name) < min_n {
            out.ensemble_weights.insert(name.clone(), *lw);
            out.live_applied.push(name.clone());
        } else {
            out.live_overridden.push(name.clone());
        }
    }
    out.live_applied.sort();
    out.live_overridden.sort();
    out.unknown.sort();
    out
}

/// Canlı ağırlık dosyasının **doğrulanmış** yükleme raporu.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveWeightsLoad {
    /// Dosyadan okunan ve **ensemble'a giren** ağırlık sayısı.
    pub applied: usize,
    /// Dosyada olmayan stratejiler (kanıt da yoksa 1.0 nötr başlangıç).
    pub defaulted: Vec<String>,
}

/// P0-A: kilidin otomatik açılması için gereken ardışık flat döngü sayısı.
pub const RISK_LOCK_FLAT_CYCLES: u32 = 2;

/// **GİRİŞ KAPISI — gerçek runtime durumu.**
///
/// `entries_allowed` bir **alan değildir**; `health_ok && !risk_locked`
/// denkleminden **türetilir**. Bu ayrım bilinçlidir: eski kodda
/// `run_cycle` içindeki auto-unlock yolu `entries_allowed = true` yazıyordu,
/// böylece `health_ok=false` iken kilit açılsa bile girişe izin veriliyordu
/// (denklem ihlali). Artık saklanacak tek şey iki bit var; türetilen
/// değer elle yazılamaz.
///
/// `Engine` bu tipi `gate` alanında TUTAR; testler de aynı tipi kullanır
/// (kopyalanmış `TestEngine` taklitleri yerine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryGate {
    /// Sağlık kapısı. `ExitsOnly` modu ve kill-switch bunu kapatır.
    health_ok: bool,
    /// Risk kilidi (flatten başarısız / black-swan sonrası kalıcı).
    risk_locked: bool,
    /// Ardışık flat doğrulama sayacı.
    flat_count: u32,
}

impl Default for EntryGate {
    /// Başlangıç değeri **açıkça**: sağlıklı, kilitsiz, sayaç sıfır →
    /// `entries_allowed = true`.
    fn default() -> Self {
        Self {
            health_ok: true,
            risk_locked: false,
            flat_count: 0,
        }
    }
}

impl EntryGate {
    /// **DENKLEMİN TEK NOKTASI.** Burada başka hiçbir yol
    /// `entries_allowed` üretmez.
    pub fn entries_allowed(&self) -> bool {
        Engine::entries_allowed_of(self.health_ok, self.risk_locked)
    }

    pub fn health_ok(&self) -> bool {
        self.health_ok
    }

    pub fn is_locked(&self) -> bool {
        self.risk_locked
    }

    pub fn flat_count(&self) -> u32 {
        self.flat_count
    }

    /// Sağlık bitini güncelle. `entries_allowed` otomatik türetilir.
    pub fn set_health_ok(&mut self, health_ok: bool) {
        self.health_ok = health_ok;
    }

    /// Kilidi AÇ: düz `entries_allowed = false` YAZILMAZ — denklem kendiliğinden
    /// `false` verir (health ne olursa olsun kilit girişe izin vermez).
    pub fn lock(&mut self) -> bool {
        self.flat_count = 0;
        let was = self.risk_locked;
        self.risk_locked = true;
        !was
    }

    /// Manuel açma. `health_ok=false` ise giriş **kapalı kalır**.
    pub fn unlock_manual(&mut self) -> bool {
        if !self.risk_locked {
            return false;
        }
        self.risk_locked = false;
        self.flat_count = 0;
        true
    }

    /// Bir döngünün auto-unlock adımı.
    ///
    /// - Kilit yoksa no-op (`false`).
    /// - Pozisyon varsa sayaç **sıfırlanır**, kilit sürer.
    /// - Pozisyon yoksa sayaç artar; `RISK_LOCK_FLAT_CYCLES` dolunca kilit
    ///   açılır ve `true` döner.
    ///
    /// `verified` = bu döngüde flat durumu **gerçek `sync_positions_from`
    /// sonucuyla** doğrulandı mı. REST taraması başarısız olduğu döngüde
    /// `false` gelir → sayaç ilerlemez (bayat pozisyon haritası "flat"
    /// görünüp kilidi erken açabilirdi).
    pub fn risk_lock_step(&mut self, any_position: bool, verified: bool) -> bool {
        if !self.risk_locked {
            return false;
        }
        if any_position {
            self.flat_count = 0; // hâlâ pozisyon var → sayaç sıfırla
            return false;
        }
        if !verified {
            return false; // doğrulanmamış flat → sayac ilerletme
        }
        self.flat_count += 1;
        if self.flat_count >= RISK_LOCK_FLAT_CYCLES {
            self.risk_locked = false;
            self.flat_count = 0;
            return true;
        }
        false
    }
}

impl Engine {
    pub async fn new(cfg_api: Config, loop_cfg: LoopConfig) -> anyhow::Result<Self> {
        let lc_cl = loop_cfg.clone();
        let mut executor = Executor::new(cfg_api.clone()).await?;
        let info = Info::new(cfg_api)?;
        let mut agg = data::Aggregator::default();
        // EĞİTİM/CANLI PENCERE EŞİTLİĞİ: eğitim `hist_bars` bar ile yapıldı.
        // Varsayılan 500'lik sabit cap, ilk kapanan mumda 4400'ü 500'e
        // indiriyordu (Elliott/Dow/GARCH uzun ufuklu mumları görmüyordu).
        // Burada aynı zaman penceresi açılır: ana interval `hist_bares`,
        // daha uzun interval'ler aynı süreyi daha az barla kapsar.
        {
            let iv = loop_cfg.interval_secs.max(1);
            agg.configure_history(
                (loop_cfg.hist_bars as u64).saturating_mul(iv),
                4500,
                iv,
            );
            let caps: Vec<String> = [60u64, 300, 900, 3600, 14_400, 86_400]
                .iter()
                .map(|i| format!("{}sn:{}", i, agg.cap_for(*i)))
                .collect();
            tracing::info!("[HIST] eğitimle eşit pencere açıldı — {}", caps.join(" "));
        }

        // TESTNET SIFIRLAMA: `ENGINE_FLATTEN=1` -> açılışta TÜM pozisyonları
        // kapat, sıfırdan başla (sanal para; hata düzeltmeleri temiz sayfa
        // ister. state recovery yine de çalışır, artık boş döner).
        if std::env::var("ENGINE_FLATTEN").as_deref() == Ok("1") {
            let mids: HashMap<String, f64> = info
                .all_mids()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(k, v)| v.parse::<f64>().ok().map(|x| (k, x)))
                .collect();
            let left = executor.flatten_all(&mids).await;
            if !left.is_empty() {
                tracing::warn!(
                    "[FLAT] {} pozisyon kapanmadı — state recovery bunları devralacak",
                    left.len()
                );
            }
        }

        // AÇILIŞ YETİM TEMİZLİĞİ: önceki çalışmadan kalan, makineye bağlı
        // OLMAYAN emirler borsada asılı kalır ve dolduğunda yönetilemez.
        // Gerçek pozisyonu olan coin'lerin emirleri DOKUNULMAZ.
        let boot_cands = executor.refresh_orders().await;
        let boot_cands = match boot_cands {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("açılış refresh_orders: {e}");
                Vec::new()
            }
        };
        // Açılışta borsada asılı kalan ÇİFT stop'ları temizle (ölçüldü:
        // önceki koşudan kalan ikişer stop aynı pozisyonu kapatmaya çalışıyordu).
        let n = executor.dedupe_sl(boot_cands).await;
        if n > 0 {
            tracing::info!("[SL] açılışta {n} artık stop iptal edildi");
        }
        {
            let orphans: Vec<(String, i64)> = executor
                .open_orders_view()
                .into_iter()
                .filter(|(cn, _)| {
                    executor
                        .positions()
                        .get(cn)
                        .map(|p| p.szi.abs() < 1e-12)
                        .unwrap_or(true)
                })
                .collect();
            for (cn, oid) in &orphans {
                match executor.cancel(cn, *oid).await {
                    Ok(_) => tracing::info!("[RECON] açılışta yetim emir iptal: {cn}/{oid}"),
                    Err(e) => tracing::warn!("açılış yetim iptal {cn}/{oid}: {e}"),
                }
            }
            if !orphans.is_empty() {
                tracing::info!("[RECON] {} yetim emir iptal edildi", orphans.len());
            }
        }
        for coin in &loop_cfg.coins {
            match agg
                .backfill(&info, coin, loop_cfg.interval_secs, loop_cfg.hist_bars)
                .await
            {
                Ok(n) => tracing::info!("{coin}: {n} mum backfill"),
                Err(e) => tracing::warn!("{coin} backfill başarısız: {e}"),
            }
        }

        // STATE RECOVERY — borsadaki gerçek pozisyonlardan
        let mut machines: Machines = HashMap::new();
        let mut recovered = 0;
        {
            let _ = executor.refresh_positions().await;
            for coin in &loop_cfg.coins {
                let existing = executor.positions().get(coin).and_then(|p| {
                    if p.szi.abs() < 1e-12 {
                        None
                    } else {
                        Some((
                            if p.szi > 0.0 { SmSide::Long } else { SmSide::Short },
                            p.entry_px,
                            p.szi.abs(),
                            p.entry_px,
                        ))
                    }
                });
                let mut m = PositionStateMachine::new(coin, SmConfig::default());
                m.recover(existing);
                if existing.is_some() {
                    recovered += 1;
                }
                machines.insert(coin.clone(), m);
            }
        }
        for coin in &loop_cfg.coins {
            machines
                .entry(coin.clone())
                .or_insert_with(|| PositionStateMachine::new(coin, SmConfig::default()));
        }
        if recovered > 0 {
            tracing::info!("state recovery: {recovered} pozisyon yüklendi");
        }

        let strategies = strategy::registry();
        // Nötr cold-start listesi: registry'deki stratejilerin ADLARI.
        // `Strategy::name` trait'i üzerinden; ağırlık dosyası yoksa bile
        // 18 strateji w=1.0 ile başlar (P0-B).
        let strategy_names: Vec<String> = strategies.iter().map(|s| s.name().to_string()).collect();
        let mut tracker = StrategyTracker::default();
        // PERSISTENCE: canlı öğrenme durumunu diskten yükle (S20).
        // Yoksa normaldir (ilk çalıştırma) → nötr cold-start devam eder.
        tracker.load_state(&std::env::var("TRACK_STATE_PATH")
            .unwrap_or_else(|_| "/root/hyperliquid/config/track_state.json".into()));

        Ok(Self {
            loop_cfg,
            executor,
            info,
            agg,
            ctx: data::market_ctx::MarketCtx::new(500),
            ensemble: Ensemble::new(EnsembleConfig { min_score: 0.30, ..Default::default() }),
            strategies,
            machines,
            books: HashMap::new(),
            tracker,
            live_weights: HashMap::new(),
            learn_pol: crate::track::LearnPolicy::default(),
            strategy_names,
            track_state_path: std::env::var("TRACK_STATE_PATH")
                .unwrap_or_else(|_| "/root/hyperliquid/config/track_state.json".into()),
            explore_on: true,
            explore_names: std::collections::HashSet::new(),
            explore_cursor: 0,
            explore_pick: None,
            gate: EntryGate::default(),
            rest_sync_verified: false,
            leverage: std::env::var("ENGINE_LEVERAGE")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|x| *x > 0.0)
                .unwrap_or(5.0),
            entry_attr: HashMap::new(),
            trail: HashMap::new(),
            atr_px: HashMap::new(),
            n_cycles: 0,
            funding_ema: HashMap::new(),
            funding_freeze_until: HashMap::new(),
            am: AllocationManager::new(AllocationConfig {
                top_n: lc_cl.top_n,
                max_position_usd: lc_cl.max_position_usd,
                ..Default::default()
            }),
            risk: crate::RiskManager::new(crate::RiskConfig {
                max_position_notional_usd: lc_cl.max_position_usd,
                max_total_notional_usd: lc_cl.max_position_usd * (lc_cl.top_n as f64 + 1.0),
                max_leverage: 5.0,
                min_account_equity_usd: 10.0,
                max_drawdown_pct: 20.0,
                max_open_orders_per_coin: 10,
                ..Default::default()
            }),
            oid_counter: 1,
            equity: 0.0,
            epoch: {
                // Her deploy/parametre değişikliğinde yeni epoch: YYYY-MM-DD-harf
                let base = chrono::Utc::now().format("%Y-%m-%d").to_string();
                format!("{}-a", base)
            },
            git_sha: {
                // build.rs ile enjekte edilebilir; yoksa "unknown"
                option_env!("GIT_SHA").unwrap_or("unknown").to_string()
            },
            trade_exits_path: std::env::var("TRADE_EXITS_PATH")
                .unwrap_or_else(|_| "/root/hyperliquid/config/trade_exits.jsonl".into()),
            last_recon_ms: 0,


        })
    }

    async fn rest_snapshot(&mut self) -> anyhow::Result<()> {
        let (meta, ctxs) = self.info.meta_and_asset_ctxs().await?;
        let names: Vec<String> = meta.universe.iter().map(|m| m.name.clone()).collect();
        self.ctx.apply_rest_snapshot(&names, &ctxs);
        Ok(())
    }

    /// WS market verisi tüketimi
    pub async fn ingest_step(&mut self, ev: data::StreamEvent) -> Vec<Action> {
        let mut acts = Vec::new();
        match ev {
            data::StreamEvent::Trade { coin, px, sz, ts, is_buy } => {
                let _closed = self.agg.on_trade(&coin, px, sz, ts, is_buy);
                let now = hyperliquid_api::now_ms();
                if let Some(m) = self.machines.get_mut(&coin) {
                    acts.push(m.on_price_tick(px, now));
                }
            }
            data::StreamEvent::Ctx { .. } => self.ctx.apply_stream(&ev),
            data::StreamEvent::Book { coin, bids, asks, .. } => {
                if let (Some(b), Some(a)) = (bids.first(), asks.first()) {
                    let bsz: f64 = bids.iter().take(5).map(|x| x.1).sum();
                    let asz: f64 = asks.iter().take(5).map(|x| x.1).sum();
                    let imb = if bsz + asz > 0.0 { (bsz - asz) / (bsz + asz) } else { 0.0 };
                    self.books.insert(coin, (b.0, a.0, imb));
                }
            }
            data::StreamEvent::Mids { .. } => {}
        }
        acts
    }

    /// WS ingest — EMİR DURUM OLAYLARI (orderUpdates/userEvents):
    /// dolum → makine; REDD → makine Flat; pozisyon kapanışı → tracker
    pub fn ingest_oe(&mut self, channel: &str, data: &serde_json::Value) {
        self.executor.handle_ws_event(channel, data);
        // (oid, coin, dolan, kalan, ort.fiyat, red mi)
        let mut evs: Vec<(i64, String, f64, f64, f64, bool)> = Vec::new();
        if channel == "orderUpdates" {
            for o in data.as_array().cloned().unwrap_or_default() {
                let oid = o["order"]["oid"].as_i64().unwrap_or(0);
                let coin = o["order"]["coin"].as_str().unwrap_or("").to_string();
                if coin.is_empty() {
                    continue;
                }
                let filled: f64 =
                    o["order"]["sz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let orig: f64 =
                    o["order"]["origSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let avg: f64 =
                    o["order"]["avgPx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let stt = o["status"].as_str().unwrap_or("");
                let rejected = stt.ends_with("Rejected")
                    || matches!(stt, "marginCanceled" | "rejected" | "canceled");
                // orderUpdates "filled" bazen sz/avgPx BOŞ geliyor (IOC tam
                // dolumunda) -> 0 dolum sanılıp makine yanlışa gidiyordu.
                // O durumda `user` kanalındaki gerçek fill kullanılacak.
                if stt == "filled" && filled > 0.0 {
                    evs.push((oid, coin, filled, (orig - filled).max(0.0), avg, false));
                } else if rejected {
                    evs.push((oid, coin, 0.0, 0.0, 0.0, true));
                }
            }
        } else {
            // `user` kanalı: gerçek dolumlar (coin, px, sz, oid) — borsa
            // ortalama dolum fiyatını BURADA verir.
            for f in self.executor.drain_fills() {
                evs.push((f.oid, f.coin, f.sz, 0.0, f.px, false));
            }
        }

        for (oid, coin, filled, remaining, avg, rejected) in evs {
            let mut exited_now = false;
            if let Some(m) = self.machines.get_mut(&coin) {
                let st_before = m.state.clone();
                if rejected {
                    let _ = m.on_rejected(oid);
                } else {
                    let _ = m.on_order_status(oid, filled, remaining, avg);
                    // PendingExit iken dolum → pozisyon kapanışı
                    if matches!(st_before, PosState::PendingExit { .. })
                        && matches!(m.state, PosState::Flat)
                    {
                        exited_now = true;
                    }
                }
            }
            if exited_now {
                if let Some(attr) = self.entry_attr.remove(&coin) {
                    // R-çarpanı: net hareket / giriş riski (PF tabanlı terfi için)
                    let move_px = match attr.side {
                        SmSide::Long => avg - attr.entry_px,
                        SmSide::Short => attr.entry_px - avg,
                    };
                    let r = move_px / attr.risk_px.max(f64::EPSILON);
                    self.tracker.record_outcome_r(
                        attr.voters,
                        hyperliquid_api::now_ms(),
                        r,
                    );
                }
            }
        }
    }

    /// **CANLI** ağırlık snapshot'ını yükle — `origin:"live"` zorunlu.
///
/// Beklenen şema (bu, `tracker_weights_json()`'ın **yazdığı** format):
/// ```json
/// { "origin":"live", "ts":…, "weights": { "Wyckoff": 0.83, "Dow Teorisi": 0.25 } }
/// ```
///
/// Doğrulama kuralları (sessiz düşüş YOK — `Err` döner, `bot.rs` açılışı
/// durdurur, çünkü "yükledim ama ensemble'a girmedi" sessizliği en kötü
/// hatadır):
/// 1. `origin == "live"` — backtest çıkışı look-ahead biaslı, reddedilir.
/// 2. `weights` bir JSON **nesnesi** olmalı.
/// 3. Her değer bir JSON **sayı** olmalı (`{"n":..,"sum_r":..}` DEĞİL —
///    canlı dosyada öyle bir şema yoktur).
/// 4. Değer `finite` ve `>= 0.0` olmalı ( negatif ağırlık kâr maximizasyonu
///    değildir; `NaN`/`Infinity` `serde_json` ile zaten parse edilmez).
///
/// **Üst sınır UYGULANMAZ**: `WEIGHT_CLAMP_HI=2.0` yalnız tracker kanıtından
/// türetilen ağırlıkları sınırlar (`effective_weights`), kaydedilmiş canlı
/// değerler **birebir** (verbatim) korunur. `1.75` dosyada varsa `1.75`
/// olarak ensemble'a girer.
pub fn load_live_weights(&mut self, raw: &serde_json::Value) -> anyhow::Result<LiveWeightsLoad> {
        let m = parse_live_weights(raw)?;
        // Dosyada olmayan stratejiler AÇIKÇA raporlanır (sessiz "nötr" yok).
        let defaulted = missing_live_strategies(&self.strategy_names, &m);
        let applied = m.len();
        self.live_weights = m;
        Ok(LiveWeightsLoad { applied, defaulted })
    }

    /// **P0-C keşif seti** — bu döngüde sırası gelen TEK strateji.
    ///
    /// `None` → boş küme (kuyruk kuyruğu boş veya kimse sinyal üretmedi).
    /// `explore_max_usd` / %5 havuz matematiği **aynen korunur**; yalnız
    /// "hangi strateji keşifçi" tanımı değişti: isim-temelli → veri-açlığı.
    pub fn explore_set(&self) -> std::collections::HashSet<String> {
        match &self.explore_pick {
            Some(n) => std::collections::HashSet::from([n.clone()]),
            None => std::collections::HashSet::new(),
        }
    }

    /// Round-robin imlecini ilerlet ve sıradaki keşifçiyi seç.
    ///
    /// Kabul kriteri: **aynı döngüde iki farklı keşifçiye sıra gelmez** —
    /// dönüş tipi `Option<String>`, tek isim. `n ≥ full_n` olan stratejiler
    /// kuyrukta görünmez (`explore_queue` filtreler) → testnet kuyruğu boş
    /// ise keşif şeridi sessizce kapalıdır (normal).
    pub fn advance_explore_cursor(&mut self, signaled: &std::collections::HashSet<String>) {
        if !self.explore_on {
            self.explore_pick = None;
            return;
        }
        let q = self.tracker.explore_queue(&self.strategy_names, &self.learn_pol);
        let act: Vec<String> = q.iter().filter(|s| signaled.contains(*s)).cloned().collect();
        self.explore_pick = if act.is_empty() {
            None
        } else {
            let pick = act[self.explore_cursor % act.len()].clone();
            self.explore_cursor = self.explore_cursor.wrapping_add(1);
            tracing::info!(
                "[KEŞİF] sıra: {} (n={} < full_n={}) | kuyruk={} | sinyal veren={} | sıradakiler={:?}",
                pick,
                self.tracker.summary(&pick).0,
                self.learn_pol.full_n,
                q.len(),
                signaled.len(),
                act
            );
            Some(pick)
        };
        // keşif kuyruğu logu (kuyruk/aktif) — hangi stratejiler n-data bekliyor
        if self.n_cycles % 6 == 0 {
            tracing::info!(
                "[KEŞİF] kuyruk (n<{}): {:?}",
                self.learn_pol.full_n,
                q.iter().take(6).collect::<Vec<_>>()
            );
        }
    }

    /// Nötr cold-start bildirimi (açılışta tek satır, P0-B).
    ///
    /// **Dürüstlük**: canlı ağırlık dosyası gerçekten ensemble'a girdiyse
    /// "nötr başlangıç" YANLIŞ olur. O durumda dosya satırı okunmuştur ve
    /// `apply_effective_weights()` uyguladığını loglar.
    pub fn log_cold_start(&self) {
        if self.live_weights.is_empty() {
            tracing::info!(
                "[LEARN] nötr başlangıç: {} strateji × w=1.0; persistence=ON (min_n={} full_n={})",
                self.strategy_names.len(),
                self.learn_pol.min_n,
                self.learn_pol.full_n
            );
            return;
        }
        // canlı ağırlık varken bu fonksiyon SESSİZ kalır:
        // "[LEARN] nötr başlangıç" satırı yanlış olurdu. Doğru bilgiyi
        // `apply_effective_weights()` yayınlar (uygulandı / cold-start kullanılmadı).
    }

    /// [LEARN] tablosu (her 60 sn'de bir — 5 sn döngüde her tur BASILMAZ).
    pub fn log_learn_table(&self) {
        let tbl = crate::track::learn_table(&self.tracker, &self.ensemble.cfg.weights);
        tracing::info!(
            "[LEARN] {} strateji | kuyruk={} | cursor={}\n{}",
            self.ensemble.cfg.weights.len(),
            self.explore_names.len(),
            self.explore_cursor,
            tbl
        );
    }

    /// Etkin ağırlıkları ensemble'a bas — **iki kaynak, açık öncelik**:
    ///
    /// 1. **Tracker kanıtı varsa** (`n >= LEARN_MIN_N`) → tracker'ın türettiği
    ///    ağırlık KAZANIR (yeni gerçek kanıt snapshot'ın üstüne yazar).
    /// 2. **Tracker kanıtı yoksa** → kaydedilmiş canlı snapshot (`live_weights`)
    ///    **korunur**; o da yoksa nötr `1.0`.
    ///
    /// Kural 2, "canlı dosya yükleniyor ama bir sonraki `apply_effective_weights`
    /// çağrısında sessizce 1.0'a eziliyor" hatasını (asıl BLOCKER) kapatır.
    /// Snapshot **birebir** uygulanır: `WEIGHT_CLAMP_*` yalnız tracker
    /// türevlerine ait, dosyadaki `1.75` `1.75` kalır.
    ///
    /// Kullanılmayan (registry'de olmayan) snapshot girdileri **sessizce
    /// düşmez** — uyarıyla loglanır.
    pub fn apply_effective_weights(&mut self) {
        let (tracker_w, explorers, events) =
            self.tracker
                .effective_weights(&self.strategy_names, &self.learn_pol);
        self.explore_names = explorers.clone();
        for e in &events {
            let v = match e.verdict {
                crate::track::ExploreVerdict::Promote => "TERFİ",
                crate::track::ExploreVerdict::Demote => "SUSTUR",
                _ => "—",
            };
            tracing::warn!(
                "[LEARN] {v}: {} {:.2}→{:.2} (n={} wr={:.2} pf={:.2})",
                e.strategy, e.from, e.to, e.trades, e.win_rate, e.profit_factor
            );
        }

        // Saf merge (testler de aynı fonksiyonu sınar).
        let merged = {
            let tracker = &self.tracker;
            merge_effective_weights(
                &tracker_w,
                &self.live_weights,
                &|name: &str| tracker.evidence_n(name),
                self.learn_pol.min_n,
            )
        };
        if !merged.unknown.is_empty() {
            tracing::warn!(
                "[LEARN] canlı ağırlık dosyasındaki {} strateji registry'de YOK → ensemble'a girmedi: {:?}",
                merged.unknown.len(),
                merged.unknown
            );
        }

        let n_w = merged.ensemble_weights.len();
        self.ensemble.cfg.weights = merged.ensemble_weights;

        if self.live_weights.is_empty() {
            tracing::info!(
                "[LEARN] nötr başlangıç: canlı ağırlık dosyası YOK → {n_w} strateji × w=1.0 (tracker kanıtı yoksa)"
            );
        } else if merged.live_applied.is_empty() {
            tracing::info!(
                "[LEARN] canlı ağırlıklar YOK sayıldı: {} strateji için tracker kanıtı yeterli (n ≥ min_n={}) → {:?}",
                merged.live_overridden.len(),
                self.learn_pol.min_n,
                merged.live_overridden
            );
        } else {
            let mut parts: Vec<String> = merged
                .live_applied
                .iter()
                .filter_map(|n| self.live_weights.get(n).map(|w| format!("{n}={w:.4}")))
                .collect();
            parts.sort();
            tracing::info!(
                "[LEARN] canlı ağırlıklar uygulandı: {} strateji [{}]",
                merged.live_applied.len(),
                parts.join(", ")
            );
            tracing::info!("[LEARN] cold-start kullanılmadı");
            if !merged.live_overridden.is_empty() {
                tracing::info!(
                    "[LEARN] canlı snapshot korundu ama tracker kanıtı üstüne yazdı ({} strateji): {:?}",
                    merged.live_overridden.len(),
                    merged.live_overridden
                );
            }
        }
    }

    /// geriye uyumlu çağrı (eski testler/yol)
    fn dyn_weights(&mut self) {
        self.apply_effective_weights();
    }

    /// Bütçe: portfolyo-paylı f* (yetersiz stat yarımda sabit)
    /// `ENGINE_BUDGET_USD` verilirse bakiye-oranı hesabı atlanır (küçük
    /// hesaplarda $72 bütçe $10 eşiğine takılıp hattı boğuyordu).
    fn budget_usd(&self) -> f64 {
        if let Ok(v) = std::env::var("ENGINE_BUDGET_USD") {
            if let Ok(x) = v.parse::<f64>() {
                if x > 0.0 {
                    return x.min(self.equity);
                }
            }
        }
        let w = self.tracker.ensemble_weights();
        let entered: Vec<&f64> =
            w.values().filter(|x| **x > 0.040 && **x != crate::track::NEUTRAL_WEIGHT).collect();
        let f = if entered.is_empty() {
            0.072
        } else {
            (entered.iter().map(|x| **x).sum::<f64>() / entered.len() as f64).min(0.10)
        };
        self.equity * f
    }

    /// ANA DÖNGÜ ADIMI (3 faz)
    /// Her döngüde BORSA STOP senkronu (S8 "exits_only" kuralı).
    ///
    /// Açık her pozisyon için borsada reduce-only SL yoksa kurar; pozisyon
    /// kapanmış (veya artık listelenmemiş) stop'u iptal eder. İdempotenttir:
    /// `sl_orders` sayesinde her döngü güvenle çağrılabilir. SL fiyatı
    /// girişte kaydedilen gerçek risk mesafesinden (`entry_attr.risk_px`)
    /// gelir; kayıt yoksa %2'lik kurtarma stop'u konur.
    pub async fn sync_exchange_sl(&mut self) {
        // Önce arka planda (spawn) kurulan stop'ların oid'ini kayda al — yoksa
        // `sl_orders` boş görünür ve aynı stop ikinci kez asılır.
        self.executor.drain_sl_results();
        // WS reconnect sonrası `sl_orders` bellekte kaybolur (ölçüldü: testnet
        // 502/connection reset). Borsa stop'ları `openOrders` taramasından
        // geri yüklemezsek motor aynı stop'u ikinci kez kuruyor.
        //
        // Tarama yalnız eksiklik varsa yapılır (döngü başına ek REST yok).
        // `dedupe_sl` aynı coin'de birden fazla stop bulursa **en yenisini
        // bırakıp eskilerini iptal eder** — ölçülen çift-stop birikimini
        // kendi kendine temizler.
        let open_pos = self
            .executor
            .positions()
            .values()
            .filter(|p| p.szi.abs() > 1e-12)
            .count();
        if self.executor.sl_orders.len() < open_pos {
            match self.executor.refresh_orders().await {
                Ok(cands) => {
                    let n = self.executor.dedupe_sl(cands).await;
                    if n > 0 {
                        tracing::info!("[SL] {n} artık stop temizlendi");
                    }
                }
                Err(e) => tracing::warn!("[SL] openOrders taraması başarısız: {e}"),
            }
        }
        let pos: Vec<(String, f64)> = self
            .executor
            .positions()
            .iter()
            .filter(|(_, p)| p.szi.abs() > 1e-12)
            .map(|(c, p)| (c.clone(), p.szi))
            .collect();
        for (coin, szi) in &pos {
            let sl_px = match self.entry_attr.get(coin) {
                Some(a) => {
                    let risk = if a.risk_px > 0.0 {
                        a.risk_px
                    } else {
                        a.entry_px.abs() * 0.02
                    };
                    if a.side.is_buy() {
                        a.entry_px - risk
                    } else {
                        a.entry_px + risk
                    }
                }
                None => {
                    // restart sonrası devralınan pozisyon: girişten %2
                    let e = self.executor.positions().get(coin).map(|p| p.entry_px).unwrap_or(0.0);
                    if !(e > 0.0) {
                        continue;
                    }
                    if *szi > 0.0 {
                        e * 0.98
                    } else {
                        e * 1.02
                    }
                }
            };
            if !(sl_px > 0.0) {
                continue;
            }
            let is_long = *szi > 0.0;

            // ================= TRAİLİNG STOP (§8.3.15) =================
            // 1) Mark ile tepe/dip güncelle, 2) hedef stop'u hesapla,
            // 3) yalnız **daha iyi** ise ve adım eşiğini geçiyorsa taşı.
            if self.loop_cfg.trail_on {
                let px = self
                    .ctx
                    .get(coin)
                    .map(|c| c.mark_px)
                    .filter(|m| *m > 0.0)
                    .or_else(|| self.executor.positions().get(coin).map(|p| p.entry_px))
                    .unwrap_or(0.0);
                let is_long = is_long;
                // ÖNCELİK: REST `entryPx` (gerçek dolum) > `entry_attr`
                // (karar anı defter fiyatı — kayma içerebilir). MFE/MAE bölmesi
                // ve tepe başlangıcı gerçek girişten olmalı.
                let pos_entry = self.executor.positions().get(coin).map(|p| p.entry_px).unwrap_or(0.0);
                let attr_entry = self.entry_attr.get(coin).map(|a| a.entry_px).unwrap_or(0.0);
                let entry_px = if pos_entry > 0.0 { pos_entry } else { attr_entry };
                // RESTART KURTARMASI: `trail` haritası RAM'de, restart'ta
                // boş. `cur_sl_px` tahminle (giriş ± ATR) dolarsa stop ilk
                // döngüde **gereksiz yere** aşağı çekilir ve koruma zayıflar.
                // `sl_last_px` borsadaki GERÇEK stop seviyesini tutar
                // (`openOrders` taraması + `ensure_sl`/`replace_sl` yazar).
                let cur_sl_init = self
                    .executor
                    .sl_last_px
                    .get(coin)
                    .copied()
                    .filter(|p| *p > 0.0)
                    .unwrap_or(sl_px);
                let t = self.trail.entry(coin.clone()).or_insert_with(|| TrailState {
                    is_long,
                    entry_px,
                    atr_px: 0.0,
                    peak_px: px.max(entry_px),
                    worst_px: px.min(entry_px),
                    last_px: px,
                    cur_sl_px: cur_sl_init,
                    opened_ms: hyperliquid_api::now_ms(),
                    ratchets: 0,
                });
                t.is_long = is_long;
                if px > 0.0 {
                    if is_long {
                        t.peak_px = t.peak_px.max(px);
                        t.worst_px = t.worst_px.min(px);
                    } else {
                        t.peak_px = t.peak_px.min(px);
                        t.worst_px = t.worst_px.max(px);
                    }
                    t.last_px = px;
                }
                // ATR güncel: yeni hesaplanmışsa al, yoksa giriş ATR'ına sabit.
                if let Some(a) = self.atr_px.get(coin).copied().filter(|a| *a > 0.0) {
                    t.atr_px = a;
                }
                if t.atr_px > 0.0 {
                    let dist = self.loop_cfg.trail_atr_k * t.atr_px;
                    let target = if is_long { t.peak_px - dist } else { t.peak_px + dist };
                    // GEVŞEME KAPISI: long için yeni stop ESKİDEN YÜKSEK,
                    // short için DAHA DÜŞÜK olmalı. Ayrıca pozisyon kapanmış
                    // stop'u (mark çok uzaklaşmış) yeniden açmayalım.
                    let better = if is_long {
                        target > t.cur_sl_px + self.loop_cfg.trail_min_step * t.atr_px
                    } else {
                        target < t.cur_sl_px - self.loop_cfg.trail_min_step * t.atr_px
                    };
                    if better && target > 0.0 {
                        match self.executor.replace_sl(coin, is_long, szi.abs(), target).await {
                            Ok(_) => {
                                let t = self.trail.get_mut(coin).unwrap();
                                t.cur_sl_px = target;
                                t.ratchets += 1;
                            }
                            Err(e) => {
                                // BAŞARISIZLIKTA ESKİ STOP AYAKTA KALIR (replace_sl
                                // sırası sayesinde) → en kötü senaryo "trailing
                                // olmadı", "korumasız pozisyon" değil.
                                tracing::warn!("[SL] {coin} trailing taşınamadı ({e}) — eski stop korunuyor");
                            }
                        }
                    }
                }
            }

            // SON SAVUNMA: kayıt düşmüşse borsada gerçekten stop var mı diye
            // bak. WS olayı (`triggered`/`canceled`) `user` dolumundan dakikalar
            // önce geliyor; aradaki boşlukta motor aynı stop'u ikinci kez
            // kuruyordu. `refresh_orders` trigger'ları `sl_orders`'a yazdığı için
            // `ensure_sl` artık idempotent.
            //
            // Trailing bu noktadan sonra `cur_sl_px`'yi de günceller: ilk
            // kurulumdan sonra hedef seviye takip edilsin.
            // `ensure_sl` YENİ stop kurduysa `cur_sl_px`'i güncelle.
            // DİKKAT: idempotent dönüşte (`Ok(false)`) GÜNCELLEME YAPILMAZ —
            // aksi halde trailing'in bir önceki satırda yeni taşıdığı seviye
            // bir sonraki satırda sabit giriş stop'una geri yazılır ve
            // trailing her döngü kendini sıfırlar (ölçüldü: hiç ratchet olmadı).
            match self.executor.ensure_sl(coin, is_long, szi.abs(), sl_px).await {
                Ok(true) => {
                    if let Some(t) = self.trail.get_mut(coin) {
                        t.cur_sl_px = sl_px;
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("[SL] {coin}: borsa stop kurulamadı: {e}"),
            }

            // GÖZLEMLENEBİLİRLİK: trailing kararının girdileri loglanır, yoksa
            // "neden ratchet olmadı" ayırt edilemiyor (gerçekte bu oldu).
            if self.loop_cfg.trail_on && self.n_cycles % 6 == 0 {
                if let Some(t) = self.trail.get(coin) {
                    tracing::info!(
                        "[TRAIL] {coin} {} mark={:.8} tepe={:.8} (entry={:.8}) ATR={:.8} stop={:.8} hedef={:.8} ratchet={}",
                        if t.is_long { "LONG" } else { "SHORT" },
                        t.last_px, t.peak_px, t.entry_px, t.atr_px, t.cur_sl_px,
                        if t.is_long { t.peak_px - self.loop_cfg.trail_atr_k * t.atr_px }
                        else { t.peak_px + self.loop_cfg.trail_atr_k * t.atr_px },
                        t.ratchets,
                    );
                }
            }
        }
        let stale: Vec<String> = self
            .executor
            .sl_orders
            .keys()
            .filter(|c| !pos.iter().any(|(p, _)| p == *c))
            .cloned()
            .collect();
        for c in stale {
            self.executor.cancel_sl(&c).await;
        }
        // Giriş emri reddedildiyse/pozisyon açılmadıysa hedef SL bayatlar.
        // ÖNEMLİ: "pozisyon yok" diye ANINDA silinemez — WS dolumu
        // `run_cycle` bitmeden işlenemediği için (ana döngü `await` sırasında
        // `oe_rx`'i okumaz) hedef, dolumdan önce temizlenip SL hiç kurulmazdı.
        // Bu yüzden 10 dakikadan eski kayıtlar temizlenir.
        let now = hyperliquid_api::now_ms();
        let stale_pending: Vec<String> = self
            .executor
            .pending_sl
            .iter()
            .filter(|(_, (_, ts))| (now as i64 - *ts as i64).unsigned_abs() > 600_000)
            .map(|(c, _)| c.clone())
            .collect();
        for c in stale_pending {
            self.executor.pending_sl.remove(&c);
            tracing::warn!("[SL] {c}: hedef stop 10 dk geçti, dolum gelmedi — temizlendi");
        }
    }

    /// ACİL DÜZLEŞTİRME (kill-switch / black-swan): tüm pozisyonları kapat.
    pub async fn flatten_positions(&mut self) {
        // Tüm açık pozisyonların exit_reason'ını EmergencyFlatten olarak işaretle
        for attr in self.entry_attr.values_mut() {
            attr.exit_reason = Some(ExitReason::EmergencyFlatten);
        }
        let mids: HashMap<String, f64> = self
            .info
            .all_mids()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, v)| v.parse::<f64>().ok().map(|x| (k, x)))
            .collect();
        let left = self.executor.flatten_all(&mids).await;
        if !left.is_empty() {
            tracing::error!("[KILL] {} pozisyon kapanamadı!", left.len());
        }
    }

    pub async fn run_cycle(&mut self) -> anyhow::Result<Vec<(String, String)>> {
        self.n_cycles += 1;
        // Bu döngüde pozisyon haritası henüz tazelenmedi: risk-lock
        // auto-unlock sayacı ilerlememeli (bayat harita "flat" görünebilir).
        self.rest_sync_verified = false;
        if let Err(e) = self.rest_snapshot().await {
            tracing::warn!("rest tarama: {e}");
        }
        if let Ok(st) = self.info.clearinghouse_state(self.executor.user()).await {
            if let Some(eq) = st["marginSummary"]["accountValue"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
            {
                self.equity = eq;
            }
            // POZİSYON HARİTASI = BORSANIN KENDİ VERİSİ. WS dolumları
            // haritayı güncelliyor ama bir WS olayı kaçırılırsa harita bayatlar
            // (ölçüldü: açılış haritası boş kaldığı için `open_now` tavanı hiç
            // tetiklenmiyordu, 3 açık pozisyon varken 4. giriş açıldı).
            // Aynı REST yanıtı zaten çekildi → buradan bedava senkron.
            if let Some(arr) = st["assetPositions"].as_array() {
                let closed = self.executor.sync_positions_from(arr);
                // P0-A: harita bu döngüde BORSADAN tazelendi → auto-unlock
                // flat doğrulaması sayacı ilerleyebilir.
                self.rest_sync_verified = true;
                // entry_attr: 24h SABİT RETENTION SESSİZ BİAS ÜRETİYOR.
                // (a) `sync_exchange_sl` SL fiyatını `risk_px`'ten alıyor; kayıt
                //     düşünce %2'lik çok gevşek kurtarma stop'u kuruluyor → 24 saatten
                //     uzun swing pozisyonunun risk profili sessizce değişiyor.
                // (b) Pozisyon kapanınca R-çarpanı hesaplanamıyor → uzun/kârlı
                //     işlemler PF tabanlı terfi matematiğinden düşüyor, yani
                //     SISTEMATİK TERS SEÇİM BİAS'ı (kazanmayan işlemler kalıyor).
                // Gerçek kural: kayıt pozisyon kapanınca düşer (aşağıda).
                // `entry_attr.entry_px` karar anındaki **defter** fiyatıdır;
                // IOC kaymasıyla dolum farklı gelir (ölçüldü: %0.9). Bu değer
                // hem `sync_exchange_sl`'in stop'unu hem R-çarpanını hem
                // MFE/MAE'yi besler → REST `entryPx` (otoriter) ile
                // üzerine yazılır. AKSI halde stop yan tarafa kurulur
                // (ölçülen: 1.1 sn'de tetiklenen kısa pozisyon).
                // P1-E: düzeltme analytical TP'yi de yeniden türetir —
                // eski entry'de kalan TP, short'ta dolumdan sonra
                // **zarar tarafına** düşebilirdi.
                let am_cfg = self.am.cfg.clone();
                let rest_entries: Vec<(String, f64)> = self
                    .executor
                    .positions()
                    .iter()
                    .map(|(cn, p)| (cn.clone(), p.entry_px))
                    .collect();
                for (cn, rest_entry) in rest_entries {
                    if rest_entry <= 0.0 {
                        continue;
                    }
                    if let Some(a) = self.entry_attr.get_mut(&cn) {
                        a.apply_real_fill(rest_entry, &am_cfg);
                    }
                }
                for c in closed {
                    // MFE/MAE (excursion) GÖSTERGESİ — TRAİLİNG'İN FAYDASINI
                    // ÖLÇMEK İÇİN. Soru: "stop daha yukarı çekilseydi bu işlem
                    // kârlı kapanır mıydı?" MFE = lehli yönde görülen en iyi
                    // fiyat, MAE = aleyhli yönde görülen en kötü fiyat. R cinsinden
                    // yazılır (risk = giriş stop mesafesi × boyut).
                    //
                    // MFE/MAE olmadan trailing'i "işe yarıyor" diye savunmak
                    // ölçümsüz iddiadır — mevcut 4 kayıp örneğinin hepsi 1-4 dk
                    // içinde stop'landı, yani MFE ≈ 0 ve trailing'in işe yaramayacağı
                    // zaten görünüyor. Bu log, bir sonraki kararı VERİYE dayandırır.
                    let excursion = self.trail.remove(&c).map(|t| {
                        let risk = (t.atr_px * self.loop_cfg.trail_atr_k).max(1e-12);
                        let fav = if t.is_long {
                            (t.peak_px - t.entry_px) / risk
                        } else {
                            (t.entry_px - t.peak_px) / risk
                        };
                        // MAE **negatif** işaretli (aleyhli yön = zarar).
                        // long: dip girişin altında → (worst - entry) < 0
                        let adv = if t.is_long {
                            (t.worst_px - t.entry_px) / risk
                        } else {
                            (t.entry_px - t.worst_px) / risk
                        };
                        (fav, adv, t)
                    });
                    if let Some((fav, adv, t)) = &excursion {
                        tracing::info!(
                            "[MFE] {c} {} MFE={:+.2}R MAE={:+.2}R süre={}sn ratchet={} son_mark={:.8}",
                            if t.is_long { "LONG" } else { "SHORT" },
                            fav,
                            adv,
                            (hyperliquid_api::now_ms() - t.opened_ms) / 1000,
                            t.ratchets,
                            t.last_px,
                        );
                    }
                    // ÇIKIŞ SEBEBİ: trail state'ten belirle (ratchets > 0 = TrailingStop)
                    // first-write-wins: zaten atanmışsa (SignalExit/EmergencyFlatten) DOKUNMA
                    let inferred = if let Some((_, _, t)) = &excursion {
                        if t.ratchets > 0 { ExitReason::TrailingStop } else { ExitReason::StopLoss }
                    } else {
                        ExitReason::StopLoss
                    };
                    let Some(mut attr) = self.entry_attr.remove(&c) else {
                        continue;
                    };
                    attr.exit_reason = attr.exit_reason.or(Some(inferred));
                    // R = net PnL / giriş riski. Pozisyon adedini bildiğimiz
                    // için risk_usd = risk_px × |szi|; `closed_pnl_by_coin`
                    // `user` dolumundaki `closedPnl` toplamıdır.
                    let sz = attr
                        .size
                        .abs()
                        .max(f64::EPSILON);
                    let risk_usd = attr.risk_px * sz;
                    let pnl = self.executor.take_closed_pnl(&c);
                    let _ = &excursion;
                    match pnl {
                        Some(p) => {
                            let r = p / risk_usd.max(f64::EPSILON);
                            self.tracker.record_outcome_r(
                                attr.voters.clone(),
                                hyperliquid_api::now_ms(),
                                r,
                            );
                            // A2 — trade_exits.jsonl yaz (append-only, epoch+git taşıyor)
                            if let Some((fav, adv, t)) = &excursion {
                                let exit_reason = attr.exit_reason.unwrap_or(ExitReason::Unknown);
                                let exit_str = match exit_reason {
                                    ExitReason::StopLoss => "stop",
                                    ExitReason::TakeProfit => "tp",
                                    ExitReason::TrailingStop => "trailing",
                                    ExitReason::SignalExit => "signal",
                                    ExitReason::EmergencyFlatten => "emergency",
                                    ExitReason::Manual => "manual",
                                    ExitReason::Unknown => "unknown",
                                };
                                let exit = TradeExit {
                                    ts: hyperliquid_api::now_ms(),
                                    epoch: self.epoch.clone(),
                                    git: self.git_sha.clone(),
                                    coin: c.clone(),
                                    side: if t.is_long { "LONG" } else { "SHORT" }.into(),
                                    strategies: attr.voters.clone(),
                                    mfe_r: *fav,
                                    mae_r: *adv,
                                    dur_sn: (hyperliquid_api::now_ms() - t.opened_ms) / 1000,
                                    ratchet: t.ratchets,
                                    cikis: exit_str.into(),
                                    r_kapanis: r,
                                    atr_bps: if t.atr_px > 0.0 { t.atr_px / t.entry_px * 10_000.0 } else { 0.0 },
                                    k: self.loop_cfg.trail_atr_k,
                                };
                                let _ = write_trade_exit(&self.trade_exits_path, &exit);
                            }
                        }
                        // WS dolumu kaçtıysa R bilinmiyor: kayıt yine de düşer
                        // (24h bias'ının kaynağı), ama yanlış R yazmak daha kötü.
                        None => tracing::warn!(
                            "[R] {c} kapandı, dolum fiyatı kaçtı — R atlandı"
                        ),
                    }
                }
            }
        }
        // `trail` haritasını da pozisyonlarla senkronla: kapanmış coinin
        // MFE/MAE kaydı kalırsa sonraki girişte yanlış tepe ile başlar.
        let open_now_coins: std::collections::HashSet<String> = self
            .executor
            .positions()
            .iter()
            .filter(|(_, p)| p.szi.abs() > 1e-12)
            .map(|(c, _)| c.clone())
            .collect();
        self.trail
            .retain(|c, _| open_now_coins.contains(c));

        // P0-A: Risk lock auto-unlock — 2 ardışık **doğrulanmış** flat döngüsü.
        // `entries_allowed` YAZILMAZ; kilit açılsa bile `health_ok=false`
        // ise giriş kapalı kalır (denklem `EntryGate`de tek yerde).
        // `verified`: bu döngüde pozisyon haritası gerçek REST
        // `sync_positions_from` ile tazelendi mi? Taranamadıysa sayaç
        // ilerlemez — bayat harita "flat" görünüp kilidi erken açardı.
        if self.gate.is_locked() {
            let any_position = self.executor.positions().values().any(|p| p.szi.abs() > 1e-12);
            if self.gate.risk_lock_step(any_position, self.rest_sync_verified) {
                tracing::warn!(
                    "[RISK-LOCK] açıldı (flat doğrulandı, döngü {})",
                    self.n_cycles
                );
            }
        }

        // Zaman filtresi artık YALNIZ pozisyonsuz artıkları toparlar: 24 saati
        // aşan ama hâlâ tutulan pozisyonun kaydı KORUNUR (aksi halde SL
        // %2'ye düşer ve R-çarpanı kaybolur).
        {
            let now0 = hyperliquid_api::now_ms();
            let stale: Vec<String> = self
                .entry_attr
                .iter()
                .filter(|(c, a)| {
                    (now0 as i64 - a.ts as i64).unsigned_abs() >= 86_400_000
                        && self
                            .executor
                            .positions()
                            .get(*c)
                            .map(|p| p.szi.abs() <= 1e-12)
                            .unwrap_or(true)
                })
                .map(|(c, _)| c.clone())
                .collect();
            for c in stale {
                self.entry_attr.remove(&c);
            }
        }
        self.dyn_weights();

        // == P1-3: S5 PERİYODİK RECON (30 sn) — cycle-sayacı yerine zaman
        // Mutex guard'ı await ÖNCESİNE serbest bırak (engine field kullan)
        let now_ms = hyperliquid_api::now_ms();
        let mut recon_notes: Vec<String> = Vec::new();
        if now_ms.saturating_sub(self.last_recon_ms) >= 30_000 {
            self.last_recon_ms = now_ms;
            let cands = self.executor.refresh_orders().await;
            let cands = match cands {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("[RECON] refresh_orders: {e}");
                    Vec::new()
                }
            };
            let n = self.executor.dedupe_sl(cands).await;
            if n > 0 {
                tracing::info!("[RECON] {n} artık stop temizlendi");
            }
        }
        // Yetim emir temizliği (S5): makineye bağlı olmayan açık emirleri iptal
        for (cn, oid) in self.executor.open_orders_view() {
            let known = self.machines.get(&cn).map(|m| {
                matches!(m.state, PosState::Pending { oid: k, .. } | PosState::PendingExit { oid: k, .. } if k == oid)
            }).unwrap_or(false);
            // BORSA STOP'U yetim DEĞİLDİR: `machines` yalnız botun kendi
            // giriş/çıkış emirlerini bilir, `sl_orders` stop'ları
            // tutar. Stop'u "yetim" sayıp iptal etmek, giriş
            // pozisyonunu borsada korumasız bırakırdı.
            let is_sl = self.executor.sl_orders.get(&cn) == Some(&oid);
            if !known && !is_sl {
                if let Err(e2) = self.executor.cancel(&cn, oid).await {
                    tracing::warn!("yetim iptal {cn}/{oid}: {e2}");
                } else {
                    recon_notes.push(format!("[RECON] yetim emir iptal: {cn}/{oid}"));
                }
            }
        }
        for a in recon_notes {
            tracing::info!("{a}");
        }

        let universe_returns: Vec<(String, f64)> = self
            .ctx
            .all()
            .filter(|c| c.prev_day_px > 0.0 && c.mark_px > 0.0)
            .map(|c| (c.coin.clone(), c.mark_px / c.prev_day_px - 1.0))
            .collect();

        // ===== FAZ 1 — TARAMA =====
        let mut evals: Vec<CycleEval> = Vec::new();
        let mut summaries: Vec<(String, String)> = Vec::new();
        for coin in self.loop_cfg.coins.clone() {
            let mut candles: Vec<data::Candle> = self
                .agg
                .history(&coin, self.loop_cfg.interval_secs)
                .to_vec();
            if let Some(cur) = self.agg.current(&coin, self.loop_cfg.interval_secs) {
                candles.push(cur);
            }
            if candles.len() < 60 {
                summaries.push((coin.clone(), "SKIP: mum yetersiz".into()));
                continue;
            }
            // P1-4: STALE-SKIP — verisi donmuş coin sinyali öğrenmeye karışmasın
            if self.agg.is_stale(&coin, self.loop_cfg.interval_secs, None) {
                summaries.push((coin.clone(), "STALE: veri donmuş".into()));
                continue;
            }
            // STALE-PROTECT: makineye kapanış tiki (trailing/timeout — WS kopsa bile döngü işler)
            {
                let mid_c = candles.last().map(|c| c.c).unwrap_or(0.0);
                if let Some(m) = self.machines.get_mut(&coin) {
                    let _ = m.on_price_tick(mid_c, hyperliquid_api::now_ms());
                }
            }
            let ctx = self.ctx.get(&coin).cloned();
            let (bid, ask, imb) = self.books.get(&coin).cloned().unwrap_or((0.0, 0.0, 0.0));
            let taker = self.agg.taker_balance(&coin).map(|(_, _, r)| r);

            let rejim_input = MarketInput {
                coin: coin.clone(),
                candles: candles.clone(),
                ctx: ctx.clone(),
                ..MarketInput::default()
            };
            let gate = classify(&rejim_input)
                .map(|(lbl, _)| lbl.gate())
                .unwrap_or(RegimeGate::Open);

            let input = MarketInput {
                coin: coin.clone(),
                candles: candles.clone(),
                ctx: ctx.clone(),
                oi_delta: self.ctx.delta_oi(&coin),
                taker_ratio: taker,
                book_imbalance: Some(imb),
                best_bid: Some(bid),
                best_ask: Some(ask),
                universe_returns: universe_returns.clone(),
                iv: None,
                equity: self.equity,
                inventory_usd: 0.0,
                order_size_usd: 0.0,
            };

            // KIYAMET KORUMASI S2: strateji panik → Skip (bot ölmez)
            let input_ref = &input;
            let signals: Vec<(String, Signal)> = self
                .strategies
                .iter()
                .map(|s| {
                    let name = s.name().to_string();
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.evaluate(input_ref))) {
                        Ok(sig) => (name, sig),
                        Err(_) => (name, Signal::Skip { reason: "strateji panik — izole".into() }),
                    }
                })
                .collect();
            let cs = self.ensemble.combine(signals);
            let cs = self.ensemble.apply_regime(cs, gate);
            summaries.push((coin.clone(), format!("{:?} skor={:+.2} | {}", cs.dir, cs.score, cs.note)));

            let h: Vec<f64> = candles.iter().map(|c| c.h).collect();
            let l: Vec<f64> = candles.iter().map(|c| c.l).collect();
            let cc: Vec<f64> = candles.iter().map(|c| c.c).collect();
            let atr_pct = {
                let a = math::indicators::atr(&h, &l, &cc, 14)
                    .last()
                    .and_then(|x| *x)
                    .unwrap_or(0.0);
                let mid = cc.last().copied().unwrap_or(0.0);
                if mid > 0.0 { a / mid } else { 0.0 }
            };
            let closes200: Vec<f64> =
                cc.iter().rev().take(200).rev().map(|x| *x).collect();
            // Mutlak ATR: trailing stop mesafesi fiyat biriminde gerekiyor
            // (`atr_pct` yüzde, stop için kullanılamaz).
            let atr_abs = math::indicators::atr(&h, &l, &cc, 14)
                .last()
                .and_then(|x| *x)
                .unwrap_or(0.0);
            if atr_abs > 0.0 {
                self.atr_px.insert(coin.clone(), atr_abs);
            }

            evals.push(CycleEval {
                coin,
                cs,
                bid,
                ask,
                atr_pct,
                closes: closes200,
            });
        }

        // ===== FAZ 2 — TAHSİS =====
        // S11 funding anomalisi: EMA + spike→cooldown (15 dk)
        {
            let now_f = hyperliquid_api::now_ms();
            for ee in &evals {
                let Some(f) = self.ctx.get(&ee.coin).map(|c| c.funding) else { continue };
                let ema = match self.funding_ema.get(&ee.coin) {
                    Some(v) => v * (2.0 / 3.0) + f * (1.0 / 3.0),
                    None => f, // ilk gözlem: freeze yok
                };
                self.funding_ema.insert(ee.coin.clone(), ema);
                if self.funding_ema.len() > 1 && ema.abs() > 1e-12 {
                    let rel = (f - ema).abs() / ema.abs().max(1e-12);
                    if rel > 4.0 {
                        self.funding_freeze_until.insert(ee.coin.clone(), now_f + 900_000);
                        tracing::warn!("{}: funding anomali (+{:.1}x) — 15dk freeze", ee.coin, rel);
                    }
                }
            }
        }
        // S14 KRÌZ TESPİTİ: ort. |getiri| >
        {
            let mut s = (0.0, 0usize);
            {
                let mut sgn_acc = 0.0;
                for e2 in &universe_returns { sgn_acc += e2.1; }
                s = (sgn_acc / universe_returns.len().max(1) as f64, universe_returns.len());
            }
            let crisis = universe_returns.iter().map(|e2| e2.1.abs()).sum::<f64>()
                / universe_returns.len().max(1) as f64 > 0.05;
            self.am.cfg.max_correlation = if crisis { 0.35 } else { crate::allocation_manager::LOOSE_CORR };
            // black-swan: işaretli ort < −%15 → TÜM pozisyonları KAPAT (gerçek flatten)
            if !universe_returns.is_empty() && s.0 < -0.15 {
                tracing::error!("BLACK SWAN: evren ort {:+.1}% — TÜM pozisyonlar kapatılıyor (gerçek flatten)", s.0 * 100.0);
                // Sadece internal state değil, BORSADA kapat (kill-switch ile aynı mekanizma)
                let mids: HashMap<String, f64> = self
                    .info
                    .all_mids()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|(k, v)| v.parse::<f64>().ok().map(|x| (k, x)))
                    .collect();
                let left = self.executor.flatten_all(&mids).await;
                if left.is_empty() {
                    // Flatten başarılı → internal state Flat
                    for m in self.machines.values_mut() {
                        m.state = PosState::Flat;
                    }
                } else {
                    // Flatten BAŞARISIZ → RISK LOCK AKTİF
                    // `entries_allowed` elle yazılmaz: `gate.lock()` yalnız
                    // kilit bitini set eder, denklem `false` verir
                    // (health_ok=false ise zaten kapalıydı).
                    // makineler DOKUNULMAZ, recovery: sync_positions_from ile borsa-temelli
                    self.gate.lock();
                    tracing::error!(
                        "[RISK-LOCK] AKTİF — flatten başarısız, kalan pozisyon: {}",
                        risk_lock_positions_left(&left)
                    );
                }
            }
        }

        let mut votes: Vec<Vote> = Vec::new();
        let (mut n_skip_neutral, mut n_skip_novoter, mut n_skip_nobook) = (0usize, 0usize, 0usize);
        let mut closes_map: HashMap<String, Vec<f64>> = HashMap::new();
        // P0-C — bu döngüde sinyal üreten stratejiler (keşif kuyruğuyla kesişim
        // için). Sinyal üretmeyene zorla işlem AÇILMAZ.
        let mut signaled: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in &evals {
            // Defter (L2) gelmemişse oy TAMAMEN atlanıyordu: 178 coin'in 173'ü
            // elendi, oy≈0 kalıyordu. Son mum kapanışı geçerli fiyattır.
            let has_book = e.bid > 1e-6 && e.ask > 1e-6;
            let last_c = e.closes.last().copied().unwrap_or(0.0);
            if !has_book && last_c <= 0.0 {
                n_skip_nobook += 1;
                continue; // ne defter ne mum — fiyat yok
            }
            let already_on = self
                .machines
                .get(&e.coin)
                .map(|m| !matches!(m.state, PosState::Flat))
                // makine yoksa pozisyon da yoktur → FLAT say (eski `true`
                // bilinmeyen coini "zaten pozisyonda" sanıyordu)
                .unwrap_or(false);
            if already_on || e.cs.dir == Dir::Neutral {
                n_skip_neutral += 1;
                continue;
            }
            let side_sign = if e.cs.dir == Dir::Long { 1.0 } else { -1.0 };
            let voters: Vec<String> = e
                .cs
                .contributors
                .iter()
                .filter(|(_, contrib)| contrib.signum() == side_sign)
                .map(|(n, _)| n.clone())
                .collect();
            if voters.is_empty() {
                n_skip_novoter += 1;
                continue;
            }
            signaled.extend(voters.iter().cloned());
            closes_map.insert(e.coin.clone(), e.closes.clone());
            // Emir defteri boşsa (dry-run'da L2 kapalıyken) mid = 0.0 olurdu
            // ve `mid > 0` filtresi TÜM oyları düşürürdü → hiç pozisyon yok.
            // Son mum kapanışına düş.
            let px = if e.cs.dir == Dir::Long { e.ask } else { e.bid };
            let mid = if px > 0.0 { px } else { e.closes.last().copied().unwrap_or(0.0) };
            votes.push(Vote {
                coin: e.coin.clone(),
                dir: e.cs.dir,
                score: e.cs.score,
                voters,
                weights: self.ensemble.cfg.weights.clone(),
                explore: self.explore_set(),
                atr_pct: e.atr_pct,
                mid,
            });
        }

        // P0-C — KEŞİF KUYRUĞU (round-robin, döngüde en fazla BİR keşifçi):
        // sıradaki keşifçi sinyaller içinde `signaled` ise min_conviction
        // bypass + $10 taban kazanır. Kuyruk = veri açlığı (`n < full_n`).
        self.advance_explore_cursor(&signaled);

        let pair_corr = |c1: &str, c2: &str| -> f64 {
            match (closes_map.get(c1), closes_map.get(c2)) {
                (Some(a), Some(b)) => {
                    let n = a.len().min(b.len());
                    if n < 30 {
                        return 0.0;
                    }
                    let (a, b) = (&a[a.len() - n..], &b[b.len() - n..]);
                    let (ma, mb) =
                        (a.iter().sum::<f64>() / n as f64, b.iter().sum::<f64>() / n as f64);
                    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
                    for i in 0..n {
                        let da = a[i] - ma;
                        let db = b[i] - mb;
                        sab += da * db;
                        saa += da * da;
                        sbb += db * db;
                    }
                    if saa > 0.0 && sbb > 0.0 {
                        sab / (saa.sqrt() * sbb.sqrt())
                    } else {
                        0.0
                    }
                }
                _ => 0.0,
            }
        };

        let budget = self.budget_usd();

        // DİNAMİK pozisyon sayısı: sabit top_n yerine bakiyeye göre —
        // her pozisyona en az `equity_per_position` ($15) düşmeli, yoksa zorunlu
        // $10 min notional altına düşüp hiç emir üretilemez.
        let cap_positions = self.risk.max_positions(self.equity, self.loop_cfg.top_n);
        let n_votes = votes.len();
        let mut selected: Vec<Candidate> = self.am.allocate(votes, &pair_corr, budget);
        if selected.len() > cap_positions {
            tracing::info!(
                "pozisyon sayısı {}→{} (equity ${:.0}, en az ${:.0}/pozisyon)",
                selected.len(), cap_positions, self.equity,
                self.risk.cfg.equity_per_position_usd
            );
            selected.truncate(cap_positions);
        }
        let sel_names: std::collections::HashSet<String> =
            selected.iter().map(|c| c.coin.clone()).collect();
        // AÇIK POZİSYON TAVANI: `cap_positions` yalnızca BU DÖNGÜDE seçilen
        // adayları sınırlıyordu, açık pozisyonları saymıyordu. Döngü 5 sn'de
        // bir çalıştığı için bot her turda yeni 3 aday açıp listeyi
        // sınırsız büyütüyordu (ölçüldü: ENGINE_TOP_N=3 iken 30 pozisyon).
        let mut open_now = self
            .executor
            .positions()
            .values()
            .filter(|p| p.szi.abs() > 1e-12)
            .count();
        tracing::info!(
            "[SEL] budget=${:.0} oy={} nötr={} oysuz={} defter yok={} selected={} ({}), keşif payı izleniyor",
                budget,
                n_votes,
                n_skip_neutral,
                n_skip_novoter,
                n_skip_nobook,
                selected.len(),
            selected.iter().map(|c| format!("{}:${:.0}(sh={:.1}%)", c.coin, c.allocated_usd, c.explorer_share * 100.0)).collect::<Vec<_>>().join(", ")
        );

        // ===== FAZ 3 — YÜRÜTME (iki aşama) =====
        let mut places: Vec<WireOrder> = Vec::new();
        let mut cancels: Vec<(u32, i64)> = Vec::new();

        // 3a) makine kararları (tek kısa ödünç alma aralığı)
        let mut acts: Vec<(usize, Action, u32, u32)> = Vec::new();
        for (idx, e) in evals.iter().enumerate() {
            if let Some((asset, szd)) = self.executor.cached_asset(&e.coin) {
                // Makine kararı da defter zorunluydu → defter yoksa HİÇ giriş
                // olmuyordu (dry-run'da 0 emir). Kapanış fiyatına düş.
                let last_c = e.closes.last().copied().unwrap_or(0.0);
                let (px_b, px_a) = if e.bid > 1e-6 && e.ask > 1e-6 {
                    (e.bid, e.ask)
                } else if last_c > 0.0 {
                    (last_c, last_c)
                } else {
                    (0.0, 0.0)
                };
                let act = if px_b > 0.0 && px_a > 0.0 {
                    self.machines
                        .get_mut(&e.coin)
                        .map(|m| m.on_decision(Decision { score: e.cs.score }, px_b, px_a, hyperliquid_api::now_ms()))
                        .unwrap_or(Action::Nothing)
                } else {
                    Action::Nothing
                };
                acts.push((idx, act, asset, szd));
            } else {
                summaries.push((e.coin.clone(), "SKIP: asset çözülemedi".into()));
            }
        }

        // risk kapısı ön güncelleme (korelasyon çöküşü + likidite denetimi)
        let mut pmap: HashMap<String, crate::executor::Position> = HashMap::new();
        for (c, m) in self.machines.iter() {
            if let Some((_s, en, size)) = m.in_position() {
                pmap.insert(
                    c.clone(),
                    crate::executor::Position {
                        position_value: (size * en).abs(),
                        ..Default::default()
                    },
                );
            }
        }
        self.risk.update_equity(self.equity);

        // oracle/mark referansı: emir fiyatı oracle bandının dışına kaçmasın
        let mark_map: HashMap<String, f64> =
            self.ctx.all().map(|c| (c.coin.clone(), c.mark_px)).collect();
        // 3b) izin → tahsis/dağıtma → yürütme (makine borçlanması kısa kısa)
        for (idx, act, asset, szd) in acts {
            let e = &evals[idx];
            match act {
                Action::Place(_o) => {
                    // P1-F EXITS_ONLY: kararsız durumda yeni giriş AÇILMAZ.
                    // Çıkışlar (`ExitNow`) ve borsa stop'ları etkilenmez.
                    if !self.entries_allowed() {
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            if m.oid_zero_pending() { m.state = PosState::Flat; }
                        }
                        continue;
                    }
                    // GERÇEK değerler: 0.0 notional + 1.0 kaldıraç geçiliyordu →
                    // pozisyon/toplam maruziyet limitleri hiç uygulanmıyordu.
                    if !sel_names.contains(&e.coin) {
                        // tahsise giremedi (korelasyon/conviction) — revert
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            if m.oid_zero_pending() { m.state = PosState::Flat; }
                        }
                        tracing::info!("{}: tahsis dışı — bekletildi", e.coin);
                        continue;
                    }
                    // AYNI COİNE TEKRAR GİRİŞ (piramitleme) YOK: pozisyon
                    // zaten varsa yeni emir üretilmez.
                    // `pmap` state machine'lerden kuruluyor ve yalnız `position_value`
                    // taşıyor — `szi` `Default` = 0.0. Burada `szi`'ye
                    // bakmak "zaten tutmuyorum" kontrolünü DAİMA false
                    // yapıyordu (ölçüldü: ZK açıkken aynı coine ikinci kez
                    // ters yönde giriş yapıldı, pozisyon ikiye katlandı).
                    // Doğru kaynak: borsadan gelen `executor.positions()`.
                    // ── P0-D: FEE-FARKINDALIKLI EV KAPISI ──────────────────
                    // Tahsisten SONRA ama yürütmeden ÖNCE: `1.5×ATR < maliyet
                    // tabanı` → SKIP. Clamp DEĞİL — clamp stop'u daraltıp
                    // EV'yi daha da bozardı.
                    //
                    // Sayılarla: round-trip ≈ 19 bps. 1.5×ATR = %0.25 (alt
                    // sınır) → R ≈ 1.3, 2:1 TP'de başabaş WR ≈ %59 → **yapısal
                    // negatif EV**. Taban = max(75 bps, 3×round_trip) = %0.75.
                    {
                        let spread_bps = {
                            // ask−bid / mid, ondalık → bps
                            let mid2 = if e.ask > 0.0 && e.bid > 0.0 {
                                (e.ask + e.bid) / 2.0
                            } else {
                                0.0
                            };
                            if mid2 > 0.0 {
                                (e.ask - e.bid).abs() / mid2 * 10_000.0
                            } else {
                                0.0
                            }
                        };
                        let (ok, risk_bps, floor_bps) =
                            crate::allocation_manager::ev_ok(
                                e.atr_pct,
                                self.am.cfg.sl_atr_mult,
                                spread_bps,
                            );
                        if !ok {
                            tracing::info!(
                                "[EV] {} ATR {:.0}bps → risk {:.0}bps < maliyet flooru {:.0}bps → SKIP \
                                 (round-trip {:.0}bps; işlem sayısının düşmesi BEKLENEN davranış)",
                                e.coin,
                                e.atr_pct * 10_000.0,
                                risk_bps,
                                floor_bps,
                                crate::allocation_manager::round_trip_cost_bps()
                            );
                            if let Some(m) = self.machines.get_mut(&e.coin) {
                                if m.oid_zero_pending() {
                                    m.state = PosState::Flat;
                                }
                            }
                            continue;
                        }
                    }
                    let already_held = self
                        .executor
                        .positions()
                        .get(&e.coin)
                        .map(|p| p.szi.abs() > 1e-12)
                        .unwrap_or(false);
                    // TAVAN DOLUYSA yeni giriş yapılmaz (çıkışlar serbest).
                    if already_held || open_now >= cap_positions {
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            if m.oid_zero_pending() {
                                m.state = PosState::Flat;
                            }
                        }
                        tracing::info!(
                            "{}: yeni giriş elendi ({} pozisyon/tavan {})",
                            e.coin,
                            open_now,
                            cap_positions
                        );
                        continue;
                    }
                    let cand_opt = selected.iter().find(|c| c.coin == e.coin);
                    let notional = cand_opt.map(|c| c.allocated_usd).unwrap_or(0.0);
                    let lev = pmap
                        .get(&e.coin)
                        .map(|p| p.leverage)
                        .unwrap_or(self.leverage)
                        .max(self.leverage);
                    if let Err(risk_err) = self.risk.pre_trade(
                        self.equity,
                        &pmap,
                        &e.coin,
                        notional,
                        lev,
                        0,
                    ) {
                        tracing::warn!("{}: RİSK KAPISI reddi: {risk_err}", e.coin);
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            if m.oid_zero_pending() { m.state = PosState::Flat; }
                        }
                        tracing::info!("{}: risk kapısı reddi — {risk_err}", e.coin);
                        continue;
                    }
                    let cand = selected.iter().find(|c| c.coin == e.coin).unwrap();
                    let book_px = if cand.dir == Dir::Long { e.ask } else { e.bid };
                    let entry_px = if book_px > 1e-6 {
                        book_px
                    } else {
                        e.closes.last().copied().unwrap_or(0.0)
                    };
                    // ATR=0 / bozuk SL koruması (min %0.25 mesafe)
                    let min_dist = entry_px * 0.0025;
                    let (sl_ok, tp_ok) = if cand.dir == Dir::Long {
                        (entry_px - cand.sl_px, cand.tp_px - entry_px)
                    } else {
                        (cand.sl_px - entry_px, entry_px - cand.tp_px)
                    };
                    if sl_ok < min_dist || tp_ok < min_dist {
                        tracing::warn!("{}: SL/TP mesafe koruması (ATR âm) — emir atlandı", e.coin);
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            if m.oid_zero_pending() { m.state = PosState::Flat; }
                        }
                        continue;
                    }
                    // GHOST ORDER + MIN NOTIONAL kapısı: `round_sz_up`'a GİRMEDEN ele.
                    // ($8 tahsis → lot 0.0000 → borsada 0-lot emir = hayalet emir)
                    if cand.allocated_usd < crate::allocation_manager::MIN_NOTIONAL_USD {
                        tracing::info!(
                            "{}: tahsis çok küçük (${:.2} < ${:.0}) — atlandı",
                            e.coin, cand.allocated_usd,
                            crate::allocation_manager::MIN_NOTIONAL_USD
                        );
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            if m.oid_zero_pending() { m.state = PosState::Flat; }
                        }
                        continue;
                    }
                    let sz = hyperliquid_api::round_sz_up(cand.allocated_usd / entry_px, szd);
                    if sz <= 0.0 || !sz.is_finite() {
                        tracing::info!(
                            "{}: lot yuvarlaması 0 (${:.2}/@${:.2}) — hayalet emir koruması, atlandı",
                            e.coin, cand.allocated_usd, entry_px
                        );
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            if m.oid_zero_pending() { m.state = PosState::Flat; }
                        }
                        continue;
                    }
                    let side_sm = if cand.dir == Dir::Long { SmSide::Long } else { SmSide::Short };
                    // KALDIRAÇ tembel: gerçek emirden hemen önce, bir kez.
                    // (Açılışta 178 istek atmak hız sınırını patlatıyordu.)
                    let lev = self.leverage as u32;
                    let coin_lv = e.coin.clone();
                    if let Err(err) = self.executor.ensure_leverage(&coin_lv, lev).await {
                        tracing::warn!("{coin_lv}: kaldıraç ayarlanamadı ({err})");
                    }
                    let mut attr = EntryAttr {
                        coin: e.coin.clone(),
                        ts: hyperliquid_api::now_ms(),
                        voters: selected.iter().find(|c| c.coin == e.coin).map(|c| c.voters.clone()).unwrap_or_default(),
                        side: side_sm,
                        entry_px,
                        risk_px: (entry_px - cand.sl_px).abs().max(entry_px * 1e-4),
                        size: sz,
                        // Analytical TP, tahsis anında karar fiyatından türetilir
                        tp_px: None,
                        exit_reason: None,
                    };
                    // P1-E: TP formülü ÜRETİMDE de aynı fonksiyondan gelir.
                    // Buradaki değer PLAN (henüz dolum yok); `apply_real_fill`
                    // gelince gerçek dolumla YENİDEN türetilir.
                    if let Some(tp) = attr.set_analytical_tp(&self.am.cfg) {
                        tracing::info!(
                            "[TP] {} analytical hedef (plan): entry={:.8} risk={:.8} TP={:.8}",
                            e.coin, entry_px, attr.risk_px, tp
                        );
                    }
                    // EMİR FİYATI: ALO(post-only) KALDIRILDI. Long'da ask'a,
                    // short'ta bid'e post-only yazmak spread'i kestiği için
                    // borsa reddediyordu ("Post only order would have
                    // immediately matched", status=badAloPxRejected) → 11
                    // emrin 4'ü hiç girmiyordu. Şimdi IOC + %0.2 kayma kapağı.
                    // Testnet defteri çok ince: ask/bid tabanlı dar kapağa hiç erişilemiyor
                    // ("could not immediately match against any resting orders").
                    // Mark varsa tabanı oracle'a bağla (kayma kapağı %0.5), yoksa
                    // defter fiyatından %0.2 kapa + %3 oracle bandı.
                    let ord_px = match mark_map.get(&e.coin) {
                        Some(mk) if *mk > 0.0 => {
                            if side_sm.is_buy() { *mk * 1.005 } else { *mk * 0.995 }
                        }
                        _ => {
                            let raw = if side_sm.is_buy() {
                                entry_px * 1.002
                            } else {
                                entry_px * 0.998
                            };
                            raw.clamp(entry_px * 0.97, entry_px * 1.03)
                        }
                    };
                    // Fiyat kapısı: kitap/closure olmayan yeni listelenmiş
                    // coinlerde entry_px=0 çıkıyor ve borsa "Order has invalid
                    // price." diyordu. Sıfır/NaN fiyatla emir ATILMAZ.
                    if !(ord_px > 1e-9) || !ord_px.is_finite() {
                        tracing::warn!(
                            "{}: fiyat geçersiz ({:.8}) — emir atlandı",
                            e.coin, ord_px
                        );
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            if m.oid_zero_pending() { m.state = PosState::Flat; }
                        }
                        continue;
                    }
                    // CLOID: `signing::post` 5xx'te isteği yeniden
                    // yollar. İstek borsada işlendi ama yanıt kaybolduysa
                    // aynı emir İKİNCİ kez yollanır ve pozisyon ikiye
                    // katlanırdı. Deterministik anahtar aynı emri retry'de
                    // aynı kılar → borsa ikinciyi "duplicate" diye reddeder.
                    // `salt = n_cycles<<16 | batch_index`: aynı döngüde aynı coin+px+sz
                    // iki kez geçerse cloid çakışmasın.
                    let cloid = crate::executor::make_cloid(
                        &e.coin,
                        ord_px,
                        sz,
                        ((self.n_cycles as u64) << 16) | (places.len() as u64 & 0xffff),
                    );
                    places.push(WireOrder {
                            asset,
                            is_buy: side_sm.is_buy(),
                            sz,
                            px: ord_px,
                            tif: Tif::Ioc,
                            reduce_only: false,
                            cloid: Some(cloid),
                        });
                    open_now += 1; // tavan sayacı bu tur için artsın
                        // BORSA STOP HEDEFİ: dolum anında `apply_fill` bunu
                        // kullanarak gecikmesiz reduce-only trigger SL kurar.
                        //
                        // ⚠️ MUTLAK FİYAT DEĞİL, **RİSK MESAFESİ** yazılır.
                        // Ölçülen hata: mutlak fiyat karar anındaki **defter**
                        // fiyatından geliyordu; IOC kayması dolumu %0.9 daha kötü
                        // getirdi → kısa pozisyonun stop'u dolumun **altında**
                        // kaldı → 1.1 saniyede tetiklendi (−$0.21). Mesafe
                        // taşınınca stop `dolum_px ± mesafe` olur ve kayma stop'u
                        // yan tarafa taşıyamaz.
                        if attr.risk_px > 0.0 {
                            self.executor
                                .pending_sl
                                .insert(e.coin.clone(), (attr.risk_px, hyperliquid_api::now_ms()));
                        }
                        self.entry_attr.insert(e.coin.clone(), attr);
                }
                Action::ExitNow(o) => {
                    // ÇIKIŞ SEBEBİ: SignalExit olarak işaretle (entry_attr'a yazılacak)
                    if let Some(attr) = self.entry_attr.get_mut(&e.coin) {
                        attr.exit_reason = Some(ExitReason::SignalExit);
                    }
                    // HAYALET EMİR KORUMASI (çıkış yolu da): makine boyutu
                    // bilmiyorsa pozisyonun kendisinden al; 0 kalırsa emir
                    // GÖNDERİLMEZ (borsada reduce-only 0-lot = kapatılmayan
                    // ya da tümünü kapatan belirsiz emir).
                    let exit_sz = if o.sz > 0.0 {
                        o.sz
                    } else {
                        // makine -> borsa sırasıyla dene
                        self.machines
                            .get(&e.coin)
                            .and_then(|m| m.in_position())
                            .map(|(_, _, sz)| sz)
                            .filter(|x| *x > 0.0)
                            .or_else(|| {
                                self.executor
                                    .positions()
                                    .get(&e.coin)
                                    .map(|p| p.szi.abs())
                                    .filter(|x| *x > 0.0)
                            })
                            .unwrap_or(0.0)
                    };
                    let xsz = hyperliquid_api::round_sz_up(exit_sz, szd);
                    if !(xsz > 0.0) || !xsz.is_finite() {
                        tracing::warn!(
                            "{}: çıkış boyutu 0 (pozisyon yok?) — kapatma emri atlandı",
                            e.coin
                        );
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            m.state = PosState::Flat;
                        }
                        continue;
                    }
                    let xpx = o.px * if o.side.is_buy() { 1.005 } else { 0.995 };
                    if !(xpx > 1e-9) || !xpx.is_finite() {
                        tracing::warn!("{}: çıkış fiyatı geçersiz — atlandı", e.coin);
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            m.state = PosState::Flat;
                        }
                        continue;
                    }
                    places.push(WireOrder {
                            asset,
                            is_buy: o.side.is_buy(),
                            sz: xsz,
                            px: xpx,
                            tif: Tif::Ioc,
                            reduce_only: true,
                            cloid: Some(crate::executor::make_cloid(
                                &e.coin, xpx, xsz,
                                ((self.n_cycles as u64) << 16) | 0x7000
                                    | (places.len() as u64 & 0x0fff),
                            )),
                        });
                }
                Action::Cancel(oid) => {
                    cancels.push((asset, oid));
                }
                _ => {}
            }
        }

        // ===== FAZ 4 — TEK ÇAĞRI YÜRÜT =====
        if !places.is_empty() || !cancels.is_empty() {
            let res = self.executor.batch_execute(&places, &cancels).await?;
            // P1-C: Exchange UNKNOWN (5xx/timeout) -> GERÇEK reconciliation.
            // Kontrol `error` string'i üzerinden DEĞİL, `BatchOut::unknown`
            // bayrağı üzerinden yapılır (string parsing ile kontrol yok).
            // Eşleşme `coin`e değil `cloid`e yapılır: aynı coin için birden
            // fazla giriş emri olabilir, coin tek başına yanlış emri bağlar.
            for r in res.iter().filter(|r| r.unknown) {
                let Some(cloid) = r.cloid.clone() else {
                    // cloid'siz emir UNKNOWN sonrası eşleştirilemez. Loglanır,
                    // machine'e dokunulmaz (pozisyon zaten borsada olabilir).
                    tracing::error!("[RECON] UNKNOWN emir cloid taşımıyor — eşleştirme yapılamadı");
                    continue;
                };
                let coin = r.coin.clone().unwrap_or_default();
                self.reconcile_unknown_cloid(&cloid, &coin).await?;
            }
        }
        // BORSA STOP SENKRONU: her döngü sonunda (açılışta devralınan
        // pozisyonlar dahil) her açık pozisyonun borsada SL'si var mı?
        self.sync_exchange_sl().await;
        Ok(summaries)
    }

    /// P1-C: UNKNOWN sonrası GERÇEK reconciliation.
    ///
    /// 1. `refresh_open_orders_all()` — TÜM dinlenen emirler (giriş dahil)
    /// 2. aynı coin **VE aynı cloid**
    /// 3. varsa `link_pending_oid(oid)`
    /// 4. yoksa machine pending → Flat
    /// 5. log
    ///
    /// `coin` tek başına YETMEZ: aynı coin için birden fazla giriş emri
    /// olabilir ve yanlış emri bağlamak pozisyonu ikiye katlar.
    pub async fn reconcile_unknown_cloid(
        &mut self,
        cloid: &str,
        coin: &str,
    ) -> anyhow::Result<()> {
        let rows = self.executor.refresh_open_orders_all().await?;
        match find_by_cloid(&rows, coin, cloid) {
            Some(oid) => {
                if let Some(m) = self.machines.get_mut(coin) {
                    m.link_pending_oid(oid);
                }
                tracing::info!("[RECON] cloid={cloid} coin={coin} oid={oid} ile bağlandı");
            }
            None => {
                if let Some(m) = self.machines.get_mut(coin) {
                    if m.oid_zero_pending() {
                        m.state = crate::state_machine::PosState::Flat;
                    }
                }
                tracing::warn!(
                    "[RECON] cloid={cloid} coin={coin} borsada YOK -> machine Flat revert"
                );
            }
        }
        Ok(())
    }
}

impl Engine {
    /// canlı etkin ağırlık (`origin:"live"` formatında — `bot.rs` geri yükler)
    pub fn tracker_weights_json(&self) -> String {
        crate::track::live_weights_json(
            &self.ensemble.cfg.weights,
            hyperliquid_api::now_ms(),
        )
    }

    /// Tracker durumunu diske yaz (P0-B persistence). Hata loglanır, yutulmaz —
    /// yutulan bir yazma hatası = tüm öğrenmenin sessizce kaybı.
    pub fn save_tracker_state(&self) {
        if let Err(e) = self.tracker.save_state(&self.track_state_path) {
            tracing::warn!(
                "[LEARN] tracker durumu yazılamadı ({}): {e} — restart'ta öğrenme KAYBOLUR",
                self.track_state_path
            );
        }
    }

    /// P0-A: giriş kapısı değişmez denklemi. `EntryGate::entries_allowed()`
    /// yalnız BUNU çağırır — yani denklem tek yerde tanımlıdır.
    pub fn entries_allowed_of(health_ok: bool, risk_locked: bool) -> bool {
        health_ok && !risk_locked
    }

    /// P0-A: Health sonucunu engin'e bildir.
    ///
    /// `entries_allowed` **artık bir alan değil**, `gate` üzerinden türetilir;
    /// burada yalnız sağlık biti güncellenir. `bot.rs` hiçbir koşulda
    /// `entries_allowed` YAZAMAZ (alan yok).
    pub fn set_health_ok(&mut self, health_ok: bool) {
        let before = self.gate.entries_allowed();
        self.gate.set_health_ok(health_ok);
        let after = self.entries_allowed();
        if before != after {
            tracing::info!(
                "[RISK-LOCK] entries_allowed={} (health_ok={}, risk_locked={})",
                after,
                self.gate.health_ok(),
                self.gate.is_locked()
            );
        }
    }

    /// Giriş kapısı — `health_ok && !risk_locked`.
    ///
    /// `entries_allowed` bir **alan** olarak hiçbir yerde saklanmaz; yalnız bu
    /// metotla türetilir. Böylece "unlock sırasında `entries_allowed = true`
    /// yazıldı" tipi ihlal yapısal olarak imkânsızdır.
    pub fn entries_allowed(&self) -> bool {
        self.gate.entries_allowed()
    }

    /// P0-A: Risk lock manuel açma (operatör müdahalesi için).
    ///
    /// Kilit açılır ama `entries_allowed` **yine** denklemden türetilir:
    /// `health_ok=false` ise giriş kapalı kalır.
    pub fn unlock_risk(&mut self) {
        if self.gate.unlock_manual() {
            tracing::warn!(
                "[RISK-LOCK] MANUEL AÇILDI — operatör müdahalesi (entries_allowed={})",
                self.entries_allowed()
            );
        }
    }
}

/// P1-C: UNKNOWN reconciliation eşleşmesi — aynı coin **VE** aynı cloid.
///
/// Ayrı fonksiyon olmasının sebebi: test ağ olmadan çalışabilmeli. Emrin
/// borsadaki `oid`'sini döndürür, yoksa `None`.
pub fn find_by_cloid(
    rows: &[crate::executor::OpenOrderRow],
    coin: &str,
    cloid: &str,
) -> Option<i64> {
    rows.iter()
        .find(|r| r.coin == coin && r.cloid.as_deref() == Some(cloid))
        .map(|r| r.oid)
}

/// Konsol özeti
pub fn fmt_signals(out: &[(String, String)]) -> String {
    out.iter()
        .map(|(c, s)| format!("{c}: {s}"))
        .collect::<Vec<_>>()
        .join(" || ")
}


#[cfg(test)]
mod trail_tests {
    use super::*;

    fn st(is_long: bool, entry: f64, atr: f64, peak: f64, cur_sl: f64) -> TrailState {
        TrailState {
            is_long,
            entry_px: entry,
            atr_px: atr,
            peak_px: peak,
            worst_px: if is_long { entry * 1.01 } else { entry * 0.99 },
            last_px: peak,
            cur_sl_px: cur_sl,
            opened_ms: 0,
            ratchets: 0,
        }
    }

    /// Trailing hedefini saf fonksiyon olarak: tepe ∓ k×ATR.
    fn target(t: &TrailState, k: f64) -> f64 {
        let d = k * t.atr_px;
        if t.is_long { t.peak_px - d } else { t.peak_px + d }
    }

    /// "daha iyi" = long için yukarı, short için aşağı, ve en az min_step×ATR.
    fn better(t: &TrailState, tgt: f64, min_step: f64) -> bool {
        if t.is_long {
            tgt > t.cur_sl_px + min_step * t.atr_px
        } else {
            tgt < t.cur_sl_px - min_step * t.atr_px
        }
    }

    #[test]
    fn long_trailing_yukarı_ratchet_eder() {
        // giriş 100, ATR 2 (k=1.5 → mesafe 3), ilk stop 97. Tepe 105'e çıktı.
        let t = st(true, 100.0, 2.0, 105.0, 97.0);
        let tgt = target(&t, 1.5);
        assert!((tgt - 102.0).abs() < 1e-9, "hedef = tepe - 1.5×ATR = 102");
        assert!(better(&t, tgt, 0.25), "stop 97'den 102'ye yükselmeli");
    }

    #[test]
    fn short_trailing_asagi_ratchet_eder() {
        let t = st(false, 100.0, 2.0, 95.0, 103.0);
        let tgt = target(&t, 1.5);
        assert!((tgt - 98.0).abs() < 1e-9, "hedef = tepe + 1.5×ATR = 98");
        assert!(better(&t, tgt, 0.25), "stop 103'ten 98'e inmeli");
    }

    #[test]
    fn trailing_asla_gevsetmez() {
        // Tepe girişe döndü: hedef eski stop'un ALTINDA kalıyor.
        let t = st(true, 100.0, 2.0, 100.0, 97.0);
        assert!(!better(&t, target(&t, 1.5), 0.25), "fiyat düşünce stop gevşemez");
        let t2 = st(false, 100.0, 2.0, 100.0, 103.0);
        assert!(!better(&t2, target(&t2, 1.5), 0.25), "short'ta da gevşemez");
    }

    #[test]
    fn min_adim_esigi_kucuk_iletlemeyi_bloklar() {
        // 0.1×ATR iyileşme: emir yakmaya değmez (her tetikleme 2 POST).
        let t = st(true, 100.0, 2.0, 100.5, 97.0);
        let tgt = target(&t, 1.5);
        assert!((tgt - 97.5).abs() < 1e-9);
        assert!(!better(&t, tgt, 0.25), "0.5 işaret < 0.25×2.0=0.5 eşiği → bloklanır");
    }

    #[test]
    fn ilk_adim_zaten_gevsetmez_giris_stopuyla_ayni_kalir() {
        // k giriş risk_px ile aynı olduğu için tepe=giriş iken hedef = giriş stop'u.
        let atr = 2.0;
        let entry = 100.0;
        let risk = 1.5 * atr; // giriş stop mesafesi
        let t = st(true, entry, atr, entry, entry - risk);
        let tgt = target(&t, 1.5);
        assert!((tgt - (entry - risk)).abs() < 1e-9, "ilk adım nötr");
        assert!(!better(&t, tgt, 0.0), "min_step=0 olsa bile eşitlik sayılır");
    }

    #[test]
    fn mfe_mae_r_cinsinden_isaretli() {
        // long: giriş 100, risk 3. Tepe 110 → MFE = +3.33R, dip 97 → MAE = -1R.
        let (entry, risk) = (100.0f64, 3.0f64);
        let (peak, worst) = (110.0f64, 97.0f64);
        let mfe = (peak - entry) / risk;
        let mae = (worst - entry) / risk; // negatif
        assert!((mfe - 3.3333).abs() < 1e-3);
        assert!((mae + 1.0).abs() < 1e-9, "MAE negatif işaretli");
    }

    #[test]
    fn short_mfe_tersi_yonlu_hesaplanir() {
        let (entry, risk) = (100.0f64, 3.0f64);
        let (peak, worst) = (90.0f64, 103.0f64);
        let mfe = (entry - peak) / risk; // lehli = aşağı
        let mae = (entry - worst) / risk; // aleyhli = yukarı → negatif
        assert!((mfe - 3.3333).abs() < 1e-3);
        assert!((mae + 1.0).abs() < 1e-9);
    }

    #[test]
    fn tp_sabit_kalir_trailing_yok() {
        // KARAR: TP ratchet EDİLMEZ. Somut örnek (giriş 100, ATR 2, risk 3):
        //   sabit TP = 106 → ödül 6, risk 3 → **3:1**
        // Fiyat tepeye (106) kadar gidip geri döndüğünde trailing TP olsaydı:
        //   tp = tepe - 1.5×ATR = 106 - 3 = 103 → ödül 3, risk 3 → **1:1**
        // Yani trailing TP, tam TERSİR (kâr satışında) 3:1'i 1:1'e düşürür —
        // kazanan işlemleri kaybettirir. Bu yüzden yalnız SL ratchet'lenir.
        // GERÇEK KOD DEĞERLERİ: `sl_atr_mult = 1.5`, `tp_atr_mult = 3.0`
        // → sabit oran 3.0/1.5 = **2:1** (dokümanda "3:1" yazıyordu, YANLIŞTI).
        let (entry, atr) = (100.0f64, 2.0f64);
        let risk = 1.5 * atr; // 3
        let tp_fixed = entry + 3.0 * atr; // 106
        let rr_fixed = (tp_fixed - entry) / risk;
        let peak = entry + 3.0 * atr; // tepe 106'ya dokundu
        let tp_trailing = peak - 1.5 * atr; // 103
        let rr_trailing = (tp_trailing - entry) / risk;
        assert!((rr_fixed - 2.0).abs() < 1e-9, "sabit TP = 2:1");
        assert!((rr_trailing - 1.0).abs() < 1e-9, "trailing TP = 1:1");
        assert!(
            rr_trailing < rr_fixed,
            "trailing TP kâr elde etme oranını düşürür — bu yüzden uygulanmıyor"
        );
        // SL trailing ise aynı senaryoda stop 97'den 103'e çıkardı:
        let sl_trailing = peak - 1.5 * atr;
        assert!(
            sl_trailing > entry - risk,
            "stop yukarı çekilir → serbest kalan risk artar (istenen davranış)"
        );
    }

}

/// ── P0-A: RISK-LOCK TESTLERİ ────────────────────────────────────────────
/// **GERÇEK runtime state**: `EntryGate` (üretimde `Engine.gate` alanı).
/// Kopyalanmış `TestEngine` YOK — testin elde ettiği tip ile motorun elde
/// ettiği tip aynıdır, dolayısıyla ihlal yapısal olarak yakalanır.
#[cfg(test)]
mod risk_lock_tests {
    use super::*;
    use crate::track::{ErrClass, Health, Mode, classify as track_classify};

    /// `EntryGate::default()` = üretimdeki `Engine::new` başlangıcı.
    fn gate() -> EntryGate {
        EntryGate::default()
    }

    /// Bir döngünün gerçek auto-unlock adımı (run_cycle'dakiyle birebir):
    /// `verified` = bu döngüde `sync_positions_from` gerçekten çalıştı mı.
    fn cycle(g: &mut EntryGate, any_position: bool, verified: bool) -> bool {
        g.risk_lock_step(any_position, verified)
    }

    /// Log sözleşmesi: `[RISK-LOCK] AKTİF` satırı pozisyonları
    /// `[(coin,sz,direction),...]` biçiminde yazar.
    #[test]
    fn risk_lock_log_format_pozisyonlari_birebir_yazar() {
        let left = vec![
            ("BTC".to_string(), 0.5),
            ("ETH".to_string(), -1.25),
        ];
        assert_eq!(
            risk_lock_positions_left(&left),
            "[(BTC,0.5,long),(ETH,-1.25,short)]"
        );
        assert_eq!(risk_lock_positions_left(&[]), "[]");
    }

    #[test]
    fn gate_baslangic_degerleri_acik_belirtilmis() {
        let g = gate();
        assert!(g.health_ok(), "başlangıç sağlıklı");
        assert!(!g.is_locked(), "başlangıç kilitsiz");
        assert_eq!(g.flat_count(), 0);
        assert!(g.entries_allowed(), "health && !locked → true");
    }

    // ── KULLANICI TESTLERİ ────────────────────────────────────────────────
    #[test]
    fn risk_lock_health_tarafindan_geri_acilamaz() {
        let mut g = gate();
        assert!(g.lock(), "lock() yeni kilidi bildirmeli");
        assert!(g.is_locked());
        assert!(!g.entries_allowed());

        // Sağlık geldi: kilit SÜRMELİ, giriş KAPALI kalmalı.
        g.set_health_ok(true);
        assert!(g.is_locked());
        assert!(
            !g.entries_allowed(),
            "health_ok=true ama risk_locked=true → entries_allowed=false"
        );
    }

    #[test]
    fn risk_lock_iki_flat_dongude_acilir() {
        let mut g = gate();
        g.lock();
        let min = RISK_LOCK_FLAT_CYCLES;
        assert_eq!(min, 2, "şart 2 ardışık flat döngü");

        for i in 1..min {
            assert!(!cycle(&mut g, false, true), "{i}. flat kilidi açmamalı");
            assert!(g.is_locked(), "{i}. flat → kilit sürmeli");
            assert!(!g.entries_allowed());
            assert_eq!(g.flat_count(), i);
        }

        assert!(cycle(&mut g, false, true), "{min}. flat → kilit açılmalı");
        assert!(!g.is_locked());
        assert_eq!(g.flat_count(), 0, "kilit açılınca sayaç sıfırlanmalı");
        assert!(g.entries_allowed(), "kilit açık + health_ok → true");
    }

    #[test]
    fn risk_lock_pozisyon_devam_ediyorsa_kalir() {
        let mut g = gate();
        g.lock();

        assert!(!cycle(&mut g, false, true));
        assert_eq!(g.flat_count(), 1);

        // Pozisyon tekrar göründü → sayaç sıfırlanır, kilit sürer.
        assert!(!cycle(&mut g, true, true));
        assert_eq!(g.flat_count(), 0, "pozisyon varsa sayaç sıfırlanmalı");
        assert!(g.is_locked());

        // Yeniden iki flat gerekir.
        assert!(!cycle(&mut g, false, true), "sıfırdan 1. flat");
        assert_eq!(g.flat_count(), 1);
        assert!(cycle(&mut g, false, true), "sıfırdan 2. flat → açılır");
        assert!(!g.is_locked(), "yeniden 2 flat sonrası açılır");

        // …ve pozisyon yine döndüyse tekrar kilitlenir, health true bile yetmez.
        assert!(g.lock());
        g.set_health_ok(true);
        assert!(!g.entries_allowed(), "pozisyon sürerken giriş kapalı");
    }

    /// ZORUNLU: kilit açılsa bile `health_ok=false` iken giriş AÇILMAZ.
    #[test]
    fn risk_lock_flat_iken_health_false_girisi_acmaz() {
        let mut g = gate();
        g.lock();
        g.set_health_ok(false); // ExitsOnly / kill-switch

        assert!(!cycle(&mut g, false, true), "1. flat");
        assert!(g.is_locked(), "1. flat → kilit sürmeli");
        assert!(cycle(&mut g, false, true), "2. flat → kilit açılır");
        assert!(!g.is_locked());
        assert!(
            !g.entries_allowed(),
            "KİLİT AÇILDI ama health_ok=false → entries_allowed=false"
        );

        // Sağlık dönene kadar de kapalı (kilit artık yok).
        assert!(!g.entries_allowed());
        g.set_health_ok(true);
        assert!(g.entries_allowed(), "sağlık dönünce giriş açılır");
    }

    /// Manuel açma da denklemi ihlal edemez.
    #[test]
    fn unlock_risk_sagliksizken_giris_acmaz() {
        let mut g = gate();
        g.lock();
        g.set_health_ok(false);
        assert!(g.unlock_manual(), "kilit açıldı");
        assert!(!g.is_locked());
        assert!(!g.entries_allowed(), "unlock_risk health=false iken giriş açmaz");

        // Zaten kilitsizken idempotent.
        assert!(!g.unlock_manual());
        assert!(!g.entries_allowed());
    }

    /// Daha güçlü invariant: denklem **her** durumda geçerli.
    #[test]
    fn entries_allowed_her_zaman_health_ve_lock_denklemidir() {
        // (a) truth table — 4 kombinasyon
        for health_ok in [true, false] {
            for locked in [true, false] {
                let mut g = gate();
                g.set_health_ok(health_ok);
                if locked {
                    g.lock();
                }
                assert_eq!(
                    g.entries_allowed(),
                    health_ok && !locked,
                    "health_ok={health_ok} risk_locked={locked}"
                );
            }
        }

        // (b) tüm runtime geçişleri — DENKLEM HER ADIMDA korunur.
        let mut g = gate();
        let mut expect = |g: &EntryGate, h: bool, l: bool| {
            assert_eq!(g.entries_allowed(), h && !l, "health_ok={h} locked={l}")
        };
        expect(&g, true, false);

        g.set_health_ok(false);
        expect(&g, false, false);
        g.set_health_ok(true);
        expect(&g, true, false);

        g.lock();
        expect(&g, true, true);
        g.set_health_ok(false);
        expect(&g, false, true);
        g.set_health_ok(true);
        expect(&g, true, true);

        // kilitli + sağlıksız → 2 flat → kilit açılır, giriş YİNE kapalı
        g.set_health_ok(false);
        expect(&g, false, true);
        assert!(!cycle(&mut g, false, true));
        expect(&g, false, true);
        assert!(cycle(&mut g, false, true), "2. flat → kilit açıldı");
        expect(&g, false, false); // <== ASIL İHLAL NOKTASI
        assert!(
            !g.entries_allowed(),
            "lock=true, health=false, flat#1, flat#2 → entries_allowed MUTLAKA false"
        );

        // manuel açma da aynı denklemi korur
        g.lock();
        assert!(!g.entries_allowed());
        g.unlock_manual();
        assert!(!g.entries_allowed(), "manuel açma da sağlıksızda giriş açmaz");
    }

    #[test]
    fn entries_allowed_denklemi_truth_table() {
        for health_ok in [true, false] {
            for locked in [true, false] {
                assert_eq!(
                    Engine::entries_allowed_of(health_ok, locked),
                    health_ok && !locked,
                    "health_ok={health_ok} risk_locked={locked}"
                );
            }
        }
    }

    /// Kilit kapalıyken `risk_lock_step` hiçbir şeye dokunmaz (kirli sayaç
    /// korunur).
    #[test]
    fn risk_lock_step_kilit_kapaliyken_etkisiz() {
        let mut g = gate();
        for _ in 0..5 {
            assert!(!cycle(&mut g, false, true));
        }
        assert!(!g.is_locked());
        assert_eq!(g.flat_count(), 0);
    }

    /// REST taraması BAŞARISIZ olduğu döngüde sayaç ilerlemez — bayat pozisyon
    /// haritası "flat" görünüp kilidi erken açamaz.
    #[test]
    fn risk_lock_dogrulanmamis_flat_sayaci_ilerletmez() {
        let mut g = gate();
        g.lock();
        for _ in 0..10 {
            assert!(!cycle(&mut g, false, false), "verified=false → kilit açılmaz");
        }
        assert!(g.is_locked(), "doğrulanmamış flat kilidi açmamalı");
        assert_eq!(g.flat_count(), 0, "doğrulanmamış flat sayacı ilerletmemeli");
        assert!(!g.entries_allowed());

        // doğrulama gelince normal 2-flat kuralı işler
        assert!(!cycle(&mut g, false, true));
        assert!(cycle(&mut g, false, true));
        assert!(!g.is_locked());
    }

    /// Yapısal koruma: `entries_allowed` **alan** olarak hiçbir yerde
    /// saklanmamalı. Kaynak taraması — asıl ihlal (`entries_allowed = true`)
    /// tekrar yazılırsa test DÜŞER.
    #[test]
    fn entries_allowed_alan_olarak_saklanmaz() {
        // Yalnız ÜRETİM kodu taranır (test modülünün kendi eşleşen
        // kalıpları yanlış pozitif üretirdi).
        let src = include_str!("engine_loop.rs");
        let src = src.split("mod risk_lock_tests").next().unwrap_or(src);
        for (i, line) in src.lines().enumerate() {
            let l = line.trim_start();
            if l.starts_with("//") || l.starts_with("///") {
                continue;
            }
            assert!(
                !l.contains("entries_allowed ="),
                "satır {}: `entries_allowed` DOĞRUDAN atanamaz (yalnız \
                 health_ok && !risk_locked): {line}",
                i + 1
            );
            assert!(
                !l.contains("entries_allowed:"),
                "satır {}: `entries_allowed` ALAN olamaz: {line}",
                i + 1
            );
        }
    }

    // ── Health taksonomisi testleri (değişmedi) ────────────────────────────
    #[test]
    fn ws_hata_firtinasi_botu_oldurmez_exits_only_gecer() {
        let mut h = Health::new(5);
        for _ in 0..15 {
            h.on_ws_reconnect();
        }
        assert_eq!(h.mode(), Mode::ExitsOnly);
        assert!(!h.entries_allowed());
        assert_ne!(h.mode(), Mode::Halted);
        assert_eq!(h.crit_streak(), 0);
    }

    #[test]
    fn reconnect_sayisi_tek_basina_kill_yapmaz() {
        let mut h = Health::new(5);
        for _ in 0..9 {
            assert!(!h.on_ws_reconnect());
        }
        assert_eq!(h.mode(), Mode::Normal);
        assert!(h.entries_allowed());
    }

    #[test]
    fn gecici_rest_hatalari_sayilmaz() {
        let mut h = Health::new(5);
        for _ in 0..20 {
            assert!(!h.on_error("502 Bad Gateway"));
            assert!(!h.on_error("connection reset by peer"));
            assert!(!h.on_error("429 Too Many Requests"));
            assert!(!h.on_error("borsa stop kurulamadı: timeout"));
        }
        assert_eq!(h.crit_streak(), 0);
        assert_eq!(h.mode(), Mode::Normal);
    }

    #[test]
    fn karar_kritik_hatalar_sayilir_ve_kill_switch_uretiyor() {
        let mut h = Health::new(5);
        for _ in 0..4 {
            assert!(!h.on_error("clearinghouseState parse edilemedi (null)"));
        }
        assert_eq!(h.mode(), Mode::Normal);
        assert!(h.on_error("clearinghouseState parse edilemedi (null)"), "5. hatada kill");
        assert_eq!(h.mode(), Mode::Halted);
    }

    #[test]
    fn basarili_dongu_sayaci_sifirlar() {
        let mut h = Health::new(5);
        for _ in 0..4 {
            h.on_error("clearinghouseState parse edilemedi");
        }
        h.on_cycle_ok();
        assert_eq!(h.crit_streak(), 0);
        for _ in 0..4 {
            assert!(!h.on_error("clearinghouseState parse edilemedi"));
        }
        assert_eq!(h.mode(), Mode::Normal);
    }

    #[test]
    fn emin_olunamayan_hata_kritik_sayilir() {
        assert_eq!(track_classify("harika bir hata"), ErrClass::Critical);
        assert_eq!(track_classify("bilinmeyen bir sey oldu"), ErrClass::Critical);
    }

    #[test]
    fn emin_olunamayan_hata_daha_spesifik_geceni_yener() {
        assert_eq!(track_classify("502 Bad Gateway"), ErrClass::Transient);
        // Üretimdeki gerçek mesaj biçimi (engine_loop.rs [SL] satırı): "stop kurulamadı"
        assert_eq!(track_classify("borsa stop kurulamadı"), ErrClass::Transient);
    }

    #[test]
    fn exits_only_kipinde_giris_yok_cikis_var() {
        let mut h = Health::new(5);
        for _ in 0..12 {
            h.on_ws_reconnect();
        }
        assert!(!h.entries_allowed());
        h.on_cycle_ok();
        h.reset();
        assert!(h.entries_allowed(), "reset -> normal");
    }

    /// P0-A'nın asıl amacı: ExitsOnly/risk-lock TEK giriş yolunu kapatır,
    /// çıkış yolları (ExitNow, SL) dokunulmadan çalışır.
    #[test]
    fn exits_only_yalnizca_place_i_bloklar() {
        use crate::state_machine::Action;
        let mut h = Health::new(5);
        for _ in 0..12 {
            h.on_ws_reconnect();
        }
        assert_eq!(h.mode(), Mode::ExitsOnly);
        assert!(!h.entries_allowed(), "giris kapali");
        // Çıkış yolları etkilenmez: yalnız Place giriş yoludur.
        use crate::state_machine::{BotOrder, Side as SmSide};
        let exit = Action::ExitNow(BotOrder {
            coin: "BTC".into(),
            side: SmSide::Short,
            sz: 1.0,
            px: 0.0,
            post_only: false,
            reduce_only: true,
        });
        assert!(!matches!(exit, Action::Place(_)), "ExitNow cikis yolu");
        assert!(matches!(Action::Cancel(1), Action::Cancel(_)));
        assert!(matches!(Action::Nothing, Action::Nothing));
    }
}

/// ── P0-B: CANLI AĞIRLIK → GERÇEK ENSEMBLE ───────────────────────────────
/// Testler saf **üretim** fonksiyonlarını sınar: `parse_live_weights` ve
/// `merge_effective_weights` (ikisi de `Engine`'in tek yoldan çağırdığı
/// fonksiyonlar — taklit/kopya yok).
#[cfg(test)]
mod live_weights_tests {
    use super::*;

    fn names(v: &[(&str, f64)]) -> Vec<String> {
        v.iter().map(|(n, _)| n.to_string()).collect()
    }

    fn live_json(pairs: &[(&str, f64)]) -> serde_json::Value {
        let mut w = serde_json::Map::new();
        for (k, v) in pairs {
            w.insert((*k).to_string(), serde_json::json!(v));
        }
        serde_json::json!({"origin": "live", "ts": 1u64, "weights": w})
    }

    /// Tracker'ın TÜM stratejiler için 1.0 (cold-start) ürettiği temel durum.
    fn tracker_cold(ns: &[String]) -> HashMap<String, f64> {
        ns.iter().map(|n| (n.clone(), 1.0)).collect()
    }

    // ── parse: canlı dosya → sayısal ağırlıklar ────────────────────────────
    #[test]
    fn live_weights_numeric_values_really_reach_ensemble() {
        let ns = names(&[("Wyckoff", 0.0), ("Dow Teorisi", 0.0)]);
        let raw = live_json(&[("Wyckoff", 1.75), ("Dow Teorisi", 0.25)]);

        let live = parse_live_weights(&raw).expect("geçerli canlı dosya");
        assert_eq!(live.get("Wyckoff").copied(), Some(1.75));
        assert_eq!(live.get("Dow Teorisi").copied(), Some(0.25));

        // engine'in effective weight pipeline'ı: tracker kanıtı YOK
        let m = merge_effective_weights(&tracker_cold(&ns), &live, &|_| 0, 10);
        assert_eq!(m.live_applied, vec!["Dow Teorisi".to_string(), "Wyckoff".to_string()]);

        // NİHAİ ENSEMBLE AĞIRLIKLARI — birebir
        assert_eq!(m.ensemble_weights.get("Wyckoff").copied(), Some(1.75));
        assert_eq!(m.ensemble_weights.get("Dow Teorisi").copied(), Some(0.25));
        // 1.75 ≠ 1.0 → dosyadaki değer ensemble'da da AYNI değerde
        assert_ne!(m.ensemble_weights.get("Wyckoff").copied(), Some(1.0));
        // üst sınır UYGULANMADI: tracker formülü clamp'liyor, canlı snapshot
        // clamp'siz — 1.75 dosyadan birebir geldi (yuvarlama bile yok).
        assert!((m.ensemble_weights["Wyckoff"] - 1.75).abs() < f64::EPSILON);
    }

    #[test]
    fn live_weights_missing_strategy_defaults_explicitly() {
        let ns = names(&[("Wyckoff", 0.0), ("Dow Teorisi", 0.0), ("Üçgen", 0.0)]);
        let live = parse_live_weights(&live_json(&[("Wyckoff", 0.5)])).unwrap();

        // Dosyada olmayan stratejiler: AÇIKÇA raporlanır (sessiz "nötr" yok).
        let defaulted = missing_live_strategies(&ns, &live);
        assert_eq!(defaulted, vec!["Dow Teorisi".to_string(), "Üçgen".to_string()]);

        let m = merge_effective_weights(&tracker_cold(&ns), &live, &|_| 0, 10);
        assert_eq!(m.ensemble_weights["Wyckoff"], 0.5, "dosyadan gelen");
        assert_eq!(m.ensemble_weights["Dow Teorisi"], 1.0, "varsayılan 1.0");
        assert_eq!(m.ensemble_weights["Üçgen"], 1.0, "varsayılan 1.0");
    }

    #[test]
    fn live_weights_origin_live_required() {
        let backtest = serde_json::json!({
            "origin": "backtest",
            "weights": {"Wyckoff": 1.75}
        });
        let e = parse_live_weights(&backtest).unwrap_err().to_string();
        assert!(e.contains("origin"), "red gerekçesi origin'i söylemeli: {e}");
        assert!(e.contains("backtest"), "red gerekçesi değeri söylemeli: {e}");

        let yok = serde_json::json!({"weights": {"Wyckoff": 1.75}});
        let e2 = parse_live_weights(&yok).unwrap_err().to_string();
        assert!(e2.contains("origin"), "origin yoksa da reddedilmeli: {e2}");

        // doğru format kabul
        assert!(parse_live_weights(&live_json(&[("Wyckoff", 1.75)])).is_ok());
    }

    #[test]
    fn live_weights_invalid_number_rejected() {
        // sayı DEĞİL (eski hatalı şema: nesne)
        let obj = serde_json::json!({
            "origin": "live",
            "weights": {"Wyckoff": {"n": 12, "sum_r": 3.0, "last_ts": 9}}
        });
        let e = parse_live_weights(&obj).unwrap_err().to_string();
        assert!(e.contains("sayı DEĞİL"), "şema hatası açık olmalı: {e}");

        // negatif
        let neg = live_json(&[("Wyckoff", -0.5)]);
        let e2 = parse_live_weights(&neg).unwrap_err().to_string();
        assert!(e2.contains("negatif"), "negatif reddedilmeli: {e2}");

        // weights nesnesi yok
        let no_w = serde_json::json!({"origin": "live"});
        let e3 = parse_live_weights(&no_w).unwrap_err().to_string();
        assert!(e3.contains("weights"), "şema yok reddedilmeli: {e3}");

        // tam sayı 0 GEÇERLİ (ölçülmemiş strateji = nötr)
        assert!(parse_live_weights(&live_json(&[("Wyckoff", 0.0)])).is_ok());
    }

    /// Cold-start `apply_effective_weights` canlı snapshot'ı **EZMEZ** —
    /// ardışık iki uygulamada da değerler durur.
    #[test]
    fn live_weights_not_overwritten_by_cold_start() {
        // "Ters" dosya yok: bir strateji canlı snapshot'ta YOK → tracker
        // cold-start 1.0 vermeli, ama ensemble listesinden **DÜŞMEMELİ**.
        let ns = names(&[("Wyckoff", 0.0), ("Dow Teorisi", 0.0), ("Üçgen", 0.0)]);
        let live = parse_live_weights(&live_json(&[("Wyckoff", 1.75), ("Dow Teorisi", 0.25)]))
            .unwrap();

        // 1. uygulama (tracker boş → cold-start 1.0)
        let m1 = merge_effective_weights(&tracker_cold(&ns), &live, &|_| 0, 10);
        assert_eq!(m1.ensemble_weights["Wyckoff"], 1.75);
        assert_eq!(m1.ensemble_weights.len(), 3, "snapshot dışı strateji düşmez");
        // 2. uygulama — hâlâ aynı
        let m2 = merge_effective_weights(&m1.ensemble_weights, &live, &|_| 0, 10);
        assert_eq!(m2.ensemble_weights["Wyckoff"], 1.75);
        assert_eq!(m2.ensemble_weights["Dow Teorisi"], 0.25);
        assert_eq!(m2.ensemble_weights["Üçgen"], 1.0, "snapshot dışı nötr kalır");
        assert_eq!(m2.ensemble_weights.len(), 3);
        assert_eq!(m2.ensemble_weights, m1.ensemble_weights, "cold-start idempotent");
    }

    /// Tracker'dan GERÇEK kanıt gelince öğrenme kazanır, snapshot ezilmez
    /// (yalnız o strateji için), bilinmeyen dosya girdisi raporlanır.
    #[test]
    fn live_weights_tracker_kaniti_ustune_yazar_bilinmeyen_raporlanir() {
        let ns = names(&[("Wyckoff", 0.0), ("Dow Teorisi", 0.0)]);
        let live = parse_live_weights(&live_json(&[
            ("Wyckoff", 1.75),
            ("Dow Teorisi", 0.25),
            ("Hayalet", 9.0),
        ]))
        .unwrap();

        // Wyckoff: 12 işlem (min_n=10) → kanıt var; Dow: 0 → kanıt yok
        let m = merge_effective_weights(
            &tracker_cold(&ns),
            &live,
            &|n: &str| if n == "Wyckoff" { 12 } else { 0 },
            10,
        );
        assert_eq!(m.live_overridden, vec!["Wyckoff".to_string()]);
        assert_eq!(m.live_applied, vec!["Dow Teorisi".to_string()]);
        assert_eq!(m.unknown, vec!["Hayalet".to_string()], "registry dışı sessiz düşmez");
        assert!(!m.ensemble_weights.contains_key("Hayalet"));
        // kanıt varsa tracker kazanır (değer cold-start 1.0)
        assert_eq!(m.ensemble_weights["Wyckoff"], 1.0);
        assert_eq!(m.ensemble_weights["Dow Teorisi"], 0.25, "kanıtsızda snapshot korunur");
    }

    /// Dosya → `tracker_weights_json()` → dosya turu şema olarak tutarlı
    /// (yazan ve okuyan aynı formatı kullanır).
    #[test]
    fn live_weights_json_round_trip_uyumlu() {
        let w: HashMap<String, f64> = HashMap::from([
            ("Wyckoff".to_string(), 1.75),
            ("Dow Teorisi".to_string(), 0.25),
        ]);
        let txt = crate::track::live_weights_json(&w, 12345);
        let raw: serde_json::Value = serde_json::from_str(&txt).expect("yazılan JSON geçerli");
        let back = parse_live_weights(&raw).expect("kendi yazdığımız dosya okunur");
        assert_eq!(back.len(), 2);
        assert!((back["Wyckoff"] - 1.75).abs() < 1e-4);
        assert!((back["Dow Teorisi"] - 0.25).abs() < 1e-4);
    }

    /// `track.rs` kanıt sayacı: `evidence_n` = `effective_weights`'in
    /// `frozen(n)` kararıyla aynı girdiyi verir.
    #[test]
    fn evidence_n_frozen_karariyla_ayni_girdiyi_verir() {
        let mut t = crate::track::StrategyTracker::default();
        let pol = crate::track::LearnPolicy { min_n: 3, full_n: 6 };
        assert_eq!(t.evidence_n("Yok"), 0);
        for i in 0..3 {
            t.record_outcome_r(vec!["Az".into()], 1_000 + i as u64, 1.0);
        }
        assert_eq!(t.evidence_n("Az"), 3);
        assert!(!pol.frozen(t.evidence_n("Az")), "n=min_n → donmuş DEĞİL");
        assert!(pol.frozen(t.evidence_n("Yok")), "kanıt yok → donmuş");
    }
}
/// P1-E: **TP_FROM_ENTRY GERÇEK AKIŞA BAĞLI** — entegrasyon testleri.
///
/// Bu modül saf formülü değil, `run_cycle`'in **çağırdığı fonksiyonları**
/// sınar: `EntryAttr::set_analytical_tp` ve `EntryAttr::apply_real_fill`.
#[cfg(test)]
mod tp_entry_tests {
    use super::*;
    use crate::allocation_manager::AllocationConfig;

    fn cfg() -> AllocationConfig {
        AllocationConfig::default()
    }

    /// ÜRETİMDEKİ GİRİŞ KAYDI (run_cycle, `Action::Place` kolu).
    fn attr(coin: &str, side: SmSide, entry_px: f64, risk_px: f64) -> EntryAttr {
        EntryAttr {
            coin: coin.to_string(),
            voters: vec!["s01".into()],
            side,
            entry_px,
            risk_px,
            size: 1.0,
            ts: 0,
            tp_px: None,
            exit_reason: None,
        }
    }

    /// P1-E ana kabul: `tp_from_entry()` **gerçek akışta** kullanılıyor.
    ///
    /// Kanıt zinciri:
    /// 1. `set_analytical_tp` → plan TP, `entry_eff = karar fiyatı`
    /// 2. `apply_real_fill(REST entryPx)` → entry düzeltilir **ve TP yeniden türetilir**
    /// 3. `attr.tp_px == cfg.tp_from_entry(dir, GERÇEK DOLUM, risk)`
    /// 4. plan TP'den **farklı** (kayma TP'yi 1:1 kaydırır) → eski fiyatta kalmıyor
    #[test]
    fn allocation_tp_entry_eff_ile_baglanir() {
        let c = cfg();
        let (decision, fill, risk) = (100.0f64, 100.9f64, 3.0f64);

        let mut a = attr("BTC", SmSide::Long, decision, risk);
        // 1) plan: karar fiyatından
        let plan = a.set_analytical_tp(&c).expect("plan TP");
        assert!((plan - c.tp_from_entry(Dir::Long, decision, risk)).abs() < 1e-12);
        assert_eq!(a.tp_px, Some(plan));

        // 2) gerçek dolum geldi → düzeltme YAPILDI
        assert!(a.apply_real_fill(fill, &c), "kayma ölçülmeliydi");
        assert_eq!(a.entry_px, fill, "entry artık gerçek dolum");

        // 3) analytical TP GERÇEK dolumdan türetildi
        let want = c.tp_from_entry(Dir::Long, fill, risk);
        assert_eq!(a.tp_px, Some(want), "TP gerçek entry_eff'ten türetilmeli");
        assert!((a.tp_px.unwrap() - (fill + 2.0 * risk)).abs() < 1e-12, "TP = dolum + 2R");

        // 4) plan TP'de kalmadı (yoksa kayma TP'yi kaydırmazdı)
        assert!((a.tp_px.unwrap() - plan).abs() > 1e-9, "TP kaymayı yansımalı");
        assert!(
            (a.tp_px.unwrap() - plan - (fill - decision)).abs() < 1e-12,
            "TP kayması 1:1 (sabit mesafe entry ile birlikte taşınır)"
        );

        // TP daima dolumun kâr tarafında
        assert!(a.tp_px.unwrap() > a.entry_px, "long TP kâr tarafında");

        // 5) short aynası: aşağı kayma → TP aşağı taşınır, hâlâ kâr tarafında
        let mut s = attr("ETH", SmSide::Short, 100.0, 3.0);
        s.set_analytical_tp(&c).expect("plan");
        assert!(s.apply_real_fill(99.1, &c), "short kayması ölçülmeliydi");
        assert_eq!(s.tp_px, Some(c.tp_from_entry(Dir::Short, 99.1, 3.0)));
        assert!(s.tp_px.unwrap() < s.entry_px, "short TP kâr tarafında");

        // 6) R:R<1 config: min 2R kapısı TP'yi yine dolumun kâr tarafında tutar
        let mut tight = cfg();
        tight.tp_atr_mult = 0.5; // R:R = 0.33
        let mut z = attr("SOL", SmSide::Short, 100.0, 10.0);
        z.set_analytical_tp(&tight).unwrap();
        assert!(z.apply_real_fill(105.0, &tight), "short yukarı kayma");
        assert!(
            z.tp_px.unwrap() < z.entry_px,
            "R:R<1 olsa bile TP dolumun kâr tarafında (min 2R kapısı)"
        );
    }

    /// Kaymasız girişte `apply_real_fill` dokunmaz → TP planla aynı kalır.
    /// Geçersiz REST fiyatı mevcut hedefi silmez.
    #[test]
    fn allocation_tp_kaymasiz_giriste_degismez() {
        let c = cfg();
        let mut a = attr("BTC", SmSide::Long, 100.0, 3.0);
        let plan = a.set_analytical_tp(&c).unwrap();
        assert!(!a.apply_real_fill(100.0, &c), "kayma yoksa düzeltme olmamalı");
        assert_eq!(a.tp_px, Some(plan));

        assert!(!a.apply_real_fill(0.0, &c));
        assert!(!a.apply_real_fill(f64::NAN, &c));
        assert_eq!(a.entry_px, 100.0);
        assert_eq!(a.tp_px, Some(plan), "geçersiz veri eski hedefi silmez");
    }

    /// Risk mesafesi kaymayla DEĞİŞMEZ (stop entry'ye göre taşınır):
    /// dolum 100→105, stop 97→102, risk 3'te kalır → TP = dolum + 2R.
    #[test]
    fn allocation_tp_kaymada_risk_mesafesi_degismez() {
        let c = cfg();
        let risk = 3.0;
        let mut a = attr("BTC", SmSide::Long, 100.0, risk);
        a.set_analytical_tp(&c).unwrap();
        a.apply_real_fill(105.0, &c);
        assert_eq!(a.risk_px, risk, "risk mesafesi değişmemeli");
        assert!((a.tp_px.unwrap() - 111.0).abs() < 1e-12, "TP = 105 + 2*3");
    }
}
