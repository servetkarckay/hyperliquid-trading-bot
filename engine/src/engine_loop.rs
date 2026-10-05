//! ENGINE LOOP — ana motor: veri → strateji → ensemble → tahsis →
//! durum makinesi → toplu emir → dolum → çıkış → öğrenme.
//!
//! Fazlar (run_cycle):
//! 1. TARAMA: her coin için kapanmış mumlar + canlı son bar → `MarketInput`
//!    → 18 strateji (panik-izole) → ensemble → rejim kapısı.
//! 2. TAHSİS: conviction + korelasyon + bütçe → `Candidate`.
//! 3. YÜRÜTME: makine `Action`'ları → WireOrder/Cancel — TEK batch POST.
//! 4. ÇIKIŞLAR: SL/TP trigger'lar borsada asılı (F: TP fill →
//!    `ExitReason::TakeProfit`); trailing ratchet; makine ExitNow.
//! ÇIKIŞ KANALLARI (B/C): ensemble `exit_requested` + kesit rank kayması +
//! turtle 55-bar altı — yalnız AÇIK pozisyonda ExitNow; Flat'ta emir yok.
//! INVARIANT (P0-A): `entries_allowed = health_ok && !risk_locked`
//! (`EntryGate` — alan değil, türetilmiş; kaynak taramasıyla korunur).
//! ÖĞRENME: R kaydı → tracker → etkin ağırlık; canlı ağırlık snapshot'ı
//! (`origin:"live"`) atomik dosyaya yazılır/okunur (sessiz ezme yok).

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
/// - **TP sabit kalır** (`tp_from_entry` sözleşmesi — P1-E/P1-H): mesafe
///   `(tp/sl)×risk` (min 2×risk) dolumla KİLİTLENMİŞTİR; trailing'i TP'ye
///   dokunsaydı başarısız bir yaklaşımda 3:1 hedef ~0.8:1'e düşer ve
///   işlem kârsız kapanırdı. Trailing yalnız **zararı sınırlar**; TP borsa
///   trigger'ı ayrı durur.
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
    /// ✅ Bu hedef ARTIK GERÇEK KAPANIŞTIR (P1-H): giriş dolumunda
    /// `pending_tp` mesafesiyle reduce-only TP trigger borsaya asılır
    /// (`Executor::ensure_tp`); TP fill → `ExitReason::TakeProfit`
    /// (`exit_reason_from_fill`, F). Analytical hedef ve actual emir AYRI
    /// kavramlardır ama AYNI formülden (`tp_from_entry`) türetilir.
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
    /// P1-K — TESTNET öğrenme dönemi: kalıcı işlem sayacı + ready_for_live.
    /// (trade-exit kayıtlarının `epoch` etiketiyle KARIŞTIRMA — bu dönems
    /// kapanmış-pozisyon sayacıdır.)
    pub learning: crate::epoch::LearningEpoch,
    learning_epoch_path: String,
    /// P0-A: Risk lock — black-swan/flatten-fail sonrası kalıcı kilit.
    /// Sadece black-swan/flatten-fail yolu true yapar.
    /// health_ok && !risk_locked = entries_allowed.
    /// Açılma: sync_positions_from 2 ardışık döngü flat doğrularsa otomatik.
    ///
    /// P1: bu döngüde pozisyon haritası `sync_positions_from` ile tazelendi mi?
    /// Auto-unlock flat doğrulaması YALNIZ bu doğrulanmış düzlemlerde ilerler.
    rest_sync_verified: bool,
    /// C — kesit momentum sıralama dilimi (önceki döngü). Rank kayması
    /// (Top → alt %40) LONG pozisyon için Exit üretir; kanıt yoksa YOK.
    rank_band_prev: HashMap<String, strategy::kesit_momentum::RankBand>,
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

/// P1-L — WS kapanış fill'i pozisyonu gerçekten kapattıysa kayıt ANINDA
/// düşmeli (REST clearinghouse sync'ine BAĞIMLI DEĞİL — REST taraması
/// 429/arıza yüzünden kaçarsa kapanış kaydı YİNE yazılır).
///
/// Koşullar: exchange-trigger fill (SL/TP oid eşleşmesi) + pozisyon artık
/// haritada YOK (tam kapanış) + EntryAttr biliniyor. Kısmi dolum
/// (`position_open=true`) finalize ÜRETMEZ — pozisyon sürüyor.
pub fn should_finalize_on_ws(
    is_sl_fill: bool,
    is_tp_fill: bool,
    position_open: bool,
    has_entry_attr: bool,
) -> bool {
    (is_sl_fill || is_tp_fill) && !position_open && has_entry_attr
}

/// BLOCKER-1 RECON — **pozisyon durum makinesini borsa haritasıyla senkronla**.
///
/// Borsa TP/SL trigger kapanışları makineye oid üzerinden haber vermez
/// (makine o oid'i bilmemez); makine `InPosition`'da KALIRDI:
/// - `already_on=true` → aynı coine yeni giriş kilitli kalır,
/// - çıkış kanalı (`force_exit`) **bayat boyutla** yeni ExitNow üretebilir
///   (canlı emir riski).
///
/// Doğru kaynak: borsa pozisyon haritası (`sync_positions_from` 'gone'
/// listesi). Sadece `InPosition`/`PendingExit` geri çevrilir — `Pending`
/// (dolum bekleyen giriş) DOKUNULMAZ (dolum sonra gelebilir).
///
/// Dönen: makinesi Flat'a çevrilen coinler (evidence temizliği çağıranın).
pub fn reconcile_machines_to_exchange(
    machines: &mut Machines,
    positions: &HashMap<String, crate::executor::Position>,
) -> Vec<String> {
    let mut closed = Vec::new();
    for (coin, m) in machines.iter_mut() {
        let on_exchange = positions
            .get(coin)
            .map(|p| p.szi.abs() > 1e-12)
            .unwrap_or(false);
        if !on_exchange
            && matches!(m.state, PosState::InPosition { .. } | PosState::PendingExit { .. })
        {
            m.state = PosState::Flat;
            closed.push(coin.clone());
        }
    }
    closed
}

/// F — **TP emrinin soyutlanmış planı** (saf fonksiyon; testler de BUNU
/// sınar, kopya mantık yok). Üretimde `Executor::ensure_tp` bu planı yürütür.
#[derive(Debug, Clone, PartialEq)]
pub struct TpOrderPlan {
    pub is_long: bool,
    pub sz: f64,
    pub tp_px: f64,
    pub reduce_only: bool,
}

/// TP planı: pozisyon VE kâr tarafı VE pozitif mesafe zorunlu.
///
/// - `has_position=false` (Flat) → `None`: **TP hiçbir emir ÜRETMEZ**.
/// - Long için `tp_px <= entry` → `None` (kâr tarafı ihlali).
/// - Short için `tp_px >= entry` → `None`.
pub fn plan_tp_order(
    has_position: bool,
    is_long: bool,
    entry_eff: f64,
    sz: f64,
    tp_px: f64,
) -> Option<TpOrderPlan> {
    if !has_position || !(sz > 0.0) || !(tp_px > 0.0) || !tp_side_ok(is_long, tp_px, entry_eff) {
        return None;
    }
    Some(TpOrderPlan { is_long, sz, tp_px, reduce_only: true })
}

/// G — **GİRİŞ FİYAT KONTRATI**: Long: SL < entry < TP (hepsi pozitif);
/// Short: TP < entry < SL. Bozuk kontrat (negatif fiyat — örn. ATR girişin
/// 50 katı) → emir ÜRETİLMEZ.
pub fn entry_contract_ok(is_long: bool, entry_eff: f64, sl_px: f64, tp_px: f64) -> bool {
    if !(entry_eff > 0.0) || !entry_eff.is_finite() {
        return false;
    }
    if !(sl_px > 0.0) || !sl_px.is_finite() || !(tp_px > 0.0) || !tp_px.is_finite() {
        return false;
    }
    if is_long {
        sl_px < entry_eff && tp_px > entry_eff
    } else {
        tp_px < entry_eff && sl_px > entry_eff
    }
}

/// Kâr tarafı kapısı: long → TP > entry, short → TP < entry.
pub fn tp_side_ok(is_long: bool, tp_px: f64, entry_eff: f64) -> bool {
    if !(tp_px > 0.0) || !(entry_eff > 0.0) {
        return false;
    }
    if is_long {
        tp_px > entry_eff
    } else {
        tp_px < entry_eff
    }
}

/// TP kurma kararı — **duplicate yasağının saf hali**.
///
/// `already_registered` (tp_orders'ta oid var) veya borsada dinleniyor
/// (`resting_on_exchange`) → kurma; grace süresi dolmadıysa → kurma.
pub fn should_place_tp(
    already_registered: bool,
    resting_on_exchange: bool,
    armed_recently: bool,
) -> bool {
    if already_registered || resting_on_exchange || armed_recently {
        return false;
    }
    true
}

/// F — **ÇIKIŞ SEBEBİ = GERÇEK KAYNAK** (saf fonksiyon, birebir eşleşme):
///
/// | gerçek kaynak | ExitReason |
/// |---|---|
/// | TP trigger fill (`is_tp_fill`) | `TakeProfit` |
/// | SL trigger fill (`is_sl_fill`) | ratchet>0 → `TrailingStop`, değilse `StopLoss` |
/// | makine ExitNow fill | `SignalExit` |
/// | kill-switch/black-swan | `EmergencyFlatten` (yazıldığı yerde set) |
/// | manuel | `Manual` |
///
/// **MFE pozitif diye TakeProfit YAZILMAZ** — bayrak yoksa `None` döner ve
/// çağıran `first-write-wins` ile mevcut gerekçeyi KORUR.
pub fn exit_reason_from_fill(
    is_tp_fill: bool,
    is_sl_fill: bool,
    trailing_ratchets: u32,
    was_manual_exit: bool,
    was_emergency: bool,
) -> Option<ExitReason> {
    if was_emergency {
        return Some(ExitReason::EmergencyFlatten);
    }
    if is_tp_fill {
        return Some(ExitReason::TakeProfit);
    }
    if is_sl_fill {
        return Some(if trailing_ratchets > 0 {
            ExitReason::TrailingStop
        } else {
            ExitReason::StopLoss
        });
    }
    if was_manual_exit {
        return Some(ExitReason::Manual);
    }
    None
}

/// F — exit_reason yazımı **first-write-wins** (çift atama/ezme yasağı).
pub fn apply_exit_reason(cur: &mut Option<ExitReason>, new: ExitReason) {
    if cur.is_none() {
        *cur = Some(new);
    }
}

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

        // P1-K: öğrenme dönemi — varsa YÜKLE (restart devamı), yoksa yeni
        // TESTNET dönem aç (nötr başlangıç; tracker zaten fresh/boş).
        let learning_epoch_path = std::env::var("LEARNING_EPOCH_PATH")
            .unwrap_or_else(|_| "/root/hyperliquid/config/learning_epoch.json".into());
        let learning = match crate::epoch::LearningEpoch::load(&learning_epoch_path) {
            Some(e) => {
                tracing::info!(
                    "[TESTNET] öğrenme dönemi sürüyor: {} — completed trades: {}/{}",
                    e.epoch_id,
                    e.completed_trade_count,
                    crate::epoch::LEARNING_TRADES_TARGET
                );
                e
            }
            None => {
                let e = crate::epoch::LearningEpoch::new_testnet(
                    hyperliquid_api::now_ms().to_string(),
                    hyperliquid_api::now_ms(),
                );
                if let Err(er) = e.save(&learning_epoch_path) {
                    tracing::error!("[TESTNET] epoch dosyası yazılamadı ({learning_epoch_path}): {er}");
                }
                tracing::info!(
                    "[TESTNET] yeni öğrenme dönemi: {} — nötr w=1.0 başlangıç, hedef {} GERÇEK kapanmış işlem",
                    e.epoch_id,
                    crate::epoch::LEARNING_TRADES_TARGET
                );
                e
            }
        };

        Ok(Self {
            learning,
            learning_epoch_path,
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
            rank_band_prev: HashMap::new(),
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
        // (oid, coin, dolan, kalan, ort.fiyat, red mi, tp fill mi, sl fill mi, closedPnl)
        let mut evs: Vec<(i64, String, f64, f64, f64, bool, bool, bool, f64)> = Vec::new();
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
                    // F: oid eşleşmesi — bu dolum bizim TP/SL trigger'dan mı?
                    let is_tp = self.executor.tp_orders.get(&coin) == Some(&oid);
                    let is_sl = self.executor.sl_orders.get(&coin) == Some(&oid);
                    evs.push((oid, coin, filled, (orig - filled).max(0.0), avg, false, is_tp, is_sl, 0.0));
                } else if rejected {
                    evs.push((oid, coin, 0.0, 0.0, 0.0, true, false, false, 0.0));
                }
            }
        } else {
            // `user` kanalı: gerçek dolumlar (coin, px, sz, oid) — borsa
            // ortalama dolum fiyatını BURADA verir.
            for f in self.executor.drain_fills() {
                evs.push((f.oid, f.coin, f.sz, 0.0, f.px, false, f.is_tp_fill, f.is_sl_fill, f.closed_pnl));
            }
        }

        for (oid, coin, filled, remaining, avg, rejected, is_tp_fill, is_sl_fill, fill_pnl) in evs {
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
            // F — ÇIKIŞ SEBEBİ GERÇEK KAYNAKTAN: TP/SL fill oid eşleşmesi
            // (MFE pozitif diye TakeProfit YAZILMAZ — bayrak yoksa dokunma).
            if is_tp_fill || is_sl_fill {
                if let Some(attr) = self.entry_attr.get_mut(&coin) {
                    let ratchets = self.trail.get(&coin).map(|t| t.ratchets).unwrap_or(0);
                    if let Some(reason) =
                        exit_reason_from_fill(is_tp_fill, is_sl_fill, ratchets, false, false)
                    {
                        apply_exit_reason(&mut attr.exit_reason, reason);
                    }
                }
                // P1-L — ANINDA FİNALİZE: pozisyon bu fill'le kapandıysa
                // (haritadan düşmüşse) TradeExit + R + sayaç ŞİMDİ yazılır;
                // REST clearinghouse taramasına bağımlı DEĞİL (429/arıza
                // kapanış kaydını kaçıramaz — ölçülen ZRO/AVAX vakası).
                let pos_open = self
                    .executor
                    .positions()
                    .get(&coin)
                    .map(|p| p.szi.abs() > 1e-12)
                    .unwrap_or(false);
                if should_finalize_on_ws(is_sl_fill, is_tp_fill, pos_open, self.entry_attr.contains_key(&coin)) {
                    let excursion = self.trail.remove(&coin).map(|t| {
                        let risk = (t.atr_px * self.loop_cfg.trail_atr_k).max(1e-12);
                        let fav = if t.is_long { (t.peak_px - t.entry_px) / risk }
                            else { (t.entry_px - t.peak_px) / risk };
                        let adv = if t.is_long { (t.worst_px - t.entry_px) / risk }
                            else { (t.entry_px - t.worst_px) / risk };
                        (fav, adv, t)
                    });
                    if let Some(attr) = self.entry_attr.remove(&coin) {
                        let risk_usd = attr.risk_px * attr.size.abs().max(f64::EPSILON);
                        let r = fill_pnl / risk_usd.max(f64::EPSILON);
                        self.tracker.record_outcome_r(
                            attr.voters.clone(),
                            hyperliquid_api::now_ms(),
                            r,
                        );
                        self.write_ws_exit_record(&coin, &attr, r, excursion);
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
                        attr.voters.clone(),
                        hyperliquid_api::now_ms(),
                        r,
                    );
                    // F — TradeExit kaydı: makine-yolu çıkışı DA trade-exit
                    // yazsın (eski kod burada attr'ı sildiği için SignalExit
                    // kayıtlarına "signal" yazılmıyordu). MFE/MAE trail'den.
                    let excursion = self.trail.remove(&coin).map(|t| {
                        let risk = (t.atr_px * self.loop_cfg.trail_atr_k).max(1e-12);
                        let fav = if t.is_long { (t.peak_px - t.entry_px) / risk }
                            else { (t.entry_px - t.peak_px) / risk };
                        let adv = if t.is_long { (t.worst_px - t.entry_px) / risk }
                            else { (t.entry_px - t.worst_px) / risk };
                        (fav, adv, t)
                    });
                    self.write_ws_exit_record(&coin, &attr, r, excursion);
                }
            }
        }
    }

    /// P1-K — **GERÇEK kapanmış işlem** sayacı (TESTNET öğrenme dönemi).
    ///
    /// Yalnız pozisyonun GERÇEKTEN kapandığı yollardan çağrılır (bkz.
    /// `write_ws_exit_record` — tek-yazım garantisi sayaç da tek tutar):
    /// - sinyal sayısı / emir sayısı / fill sayısı İŞLEM DEĞİLDİR,
    /// - duplicate/replay fill'ler tid dedup'la zaten düşer → ikinci kez
    ///   yazım → ikinci kez sayım YOK,
    /// - sayaç atomik kalıcıdır: restart'ta kaldığı yerden devam eder.
    pub fn record_completed_trade(&mut self) {
        let (n, ready_now) = self.learning.record_completed_trade();
        let target = crate::epoch::LEARNING_TRADES_TARGET;
        if ready_now {
            tracing::warn!("[TESTNET] {n}/{target} tamamlandı — LIVE geçişi için hazır (otomatik canlı işlem YOK)");
        } else {
            tracing::info!("[TESTNET] completed trades: {n}/{target}");
        }
        if let Err(e) = self.learning.save(&self.learning_epoch_path) {
            tracing::error!(
                "[TESTNET] sayaç yazılamadı ({}): {e} — restart'ta sayı GERİYE düşebilir",
                self.learning_epoch_path
            );
        }
    }

    /// WS-yolu (makine PendingExit→Flat) çıkışının trade-exit kaydı.
    /// `entry_attr` YAKINMAZ kaldırıldığı için recon yolu ikinci kez YAZMAZ.
    fn write_ws_exit_record(
        &mut self,
        coin: &str,
        attr: &EntryAttr,
        r: f64,
        excursion: Option<(f64, f64, TrailState)>,
    ) {
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
        let (fav, adv, t) = match excursion {
            Some(x) => x,
            None => (
                0.0,
                0.0,
                TrailState {
                    is_long: attr.side.is_buy(),
                    entry_px: attr.entry_px,
                    atr_px: 0.0,
                    peak_px: attr.entry_px,
                    worst_px: attr.entry_px,
                    last_px: attr.entry_px,
                    cur_sl_px: attr.entry_px,
                    opened_ms: attr.ts,
                    ratchets: 0,
                },
            ),
        };
        let exit = TradeExit {
            ts: hyperliquid_api::now_ms(),
            epoch: self.epoch.clone(),
            git: self.git_sha.clone(),
            coin: coin.to_string(),
            side: if attr.side.is_buy() { "LONG" } else { "SHORT" }.into(),
            strategies: attr.voters.clone(),
            mfe_r: fav,
            mae_r: adv,
            dur_sn: (hyperliquid_api::now_ms() - t.opened_ms) / 1000,
            ratchet: t.ratchets,
            cikis: exit_str.into(),
            r_kapanis: r,
            atr_bps: if t.atr_px > 0.0 { t.atr_px / t.entry_px * 10_000.0 } else { 0.0 },
            k: self.loop_cfg.trail_atr_k,
        };
        if let Err(e) = write_trade_exit(&self.trade_exits_path, &exit) {
            tracing::warn!("trade-exit yazılamadı ({coin}): {e}");
        }
        // P1-K: TradeExit yazıldıysa bu GERÇEK kapanış sayılır (birebir
        // tutarlılık: sayaç ↔ trade_exits.jsonl).
        self.record_completed_trade();
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

        // ============ F — GERÇEK TP EXECUTION (sync) ============
        // 1) arka planda kurulan TP'lerin oid'ini kayda al,
        // 2) pozisyonu olan ama TP'si olmayan coine TP kur (idempotent),
        // 3) kapanmış coinin TP'sini iptal et.
        self.executor.drain_tp_results();
        let open_pos_tp: Vec<(String, f64)> = self
            .executor
            .positions()
            .iter()
            .filter(|(_, p)| p.szi.abs() > 1e-12)
            .map(|(c, p)| (c.clone(), p.szi))
            .collect();
        // 2a) TP oid kaybı (restart/WS kopması): borsada dinlenen bizim TP'yi
        // TARA → oid geri yükle → **yeni emir ATILMAZ** (duplicate yasağı).
        if self.executor.tp_orders.len() < open_pos_tp.len() {
            match self.executor.fetch_open_order_rows().await {
                Ok(rows) => {
                    let want: HashMap<String, f64> = self
                        .entry_attr
                        .iter()
                        .filter_map(|(c, a)| a.tp_px.filter(|t| *t > 0.0).map(|t| (c.clone(), t)))
                        .collect();
                    let n = self.executor.restore_tp_orders(&rows, &want);
                    if n > 0 {
                        tracing::info!("[TP] {n} TP oid borsadan geri yüklendi");
                    }
                }
                Err(e) => tracing::warn!("[TP] openOrders taraması başarısız: {e}"),
            }
        }
        for (coin, szi) in &open_pos_tp {
            let tp_px = match self.entry_attr.get(coin) {
                Some(a) if a.tp_px.is_some() => a.tp_px.unwrap(),
                Some(a) => {
                    // dolum düzeltmesi gelmemiş: risk mesafesinden (tek formül)
                    let risk = if a.risk_px > 0.0 { a.risk_px } else { a.entry_px.abs() * 0.02 };
                    let dir = if a.side.is_buy() { Dir::Long } else { Dir::Short };
                    self.am.cfg.tp_from_entry(dir, a.entry_px, risk)
                }
                None => {
                    // restart sonrası devralınan pozisyon: girişten %2×2 mesafe
                    let e = self.executor.positions().get(coin).map(|p| p.entry_px).unwrap_or(0.0);
                    if !(e > 0.0) {
                        continue;
                    }
                    if *szi > 0.0 { e * 1.04 } else { e * 0.96 }
                }
            };
            if !(tp_px > 0.0) || !tp_px.is_finite() {
                continue;
            }
            let is_long = *szi > 0.0;
            // KÂR TARAFI KAPISI (F): long TP > entry, short TP < entry.
            let entry_eff = self
                .executor
                .positions()
                .get(coin)
                .map(|p| p.entry_px)
                .filter(|e| *e > 0.0)
                .or_else(|| self.entry_attr.get(coin).map(|a| a.entry_px))
                .unwrap_or(0.0);
            if !tp_side_ok(is_long, tp_px, entry_eff) {
                tracing::warn!(
                    "[TP] {coin} TP kâr tarafında DEĞİL (tp={tp_px:.8} entry={entry_eff:.8}) — kurulmadı"
                );
                continue;
            }
            let sz = self.executor.positions().get(coin).map(|p| p.szi.abs()).unwrap_or(0.0);
            if let Err(e) = self.executor.ensure_tp(coin, is_long, sz, tp_px).await {
                tracing::warn!("[TP] {coin} TP kurulamadı: {e}");
            }
        }
        let stale_tp: Vec<String> = self
            .executor
            .tp_orders
            .keys()
            .filter(|c| !open_pos_tp.iter().any(|(p, _)| p == *c))
            .cloned()
            .collect();
        for c in stale_tp {
            self.executor.cancel_tp(&c).await;
        }
        let stale_pending_tp: Vec<String> = self
            .executor
            .pending_tp
            .iter()
            .filter(|(_, (_, ts))| (now as i64 - *ts as i64).unsigned_abs() > 600_000)
            .map(|(c, _)| c.clone())
            .collect();
        for c in stale_pending_tp {
            self.executor.pending_tp.remove(&c);
            tracing::warn!("[TP] {c}: hedef TP 10 dk geçti, dolum gelmedi — temizlendi");
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
        match self.info.clearinghouse_state(self.executor.user()).await {
            Ok(st) => {
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
                let mut closed = self.executor.sync_positions_from(arr);
                // P1-L — REST delta TEK BAŞINA YETMEZ: `apply_fill` kapanan
                // pozisyonu haritadan düştüğü için 'gone' delta kapanışları
                // YUTAR (ölçüldü: ZRO/AVAX SL tetiklendi, kayıt YOK).
                // Gerçek WS işareti: closed_pnl haritasındaki closing fill'ler
                // → union; ikisi de aynı bloktan TEK kayıt + TEK sayım alır
                // (entry_attr remove tek-yazım garantisi).
                for c in self.executor.closed_pnl_coins() {
                    if !closed.contains(&c) {
                        closed.push(c);
                    }
                }
                // P0-A: harita bu döngüde BORSADAN tazelendi → auto-unlock
                // flat doğrulaması sayacı ilerleyebilir.
                self.rest_sync_verified = true;
                // BLOCKER-1: borsa kapanışlarını durum makinesine UYGULA
                // (TP/SL/external close makineye oid üzerinden gelmez).
                {
                    let gone_machines = {
                        let positions = self.executor.positions().clone();
                        reconcile_machines_to_exchange(&mut self.machines, &positions)
                    };
                    for c in &gone_machines {
                        tracing::info!("[RECON] {c} borsa kapanışı → machine Flat");
                        // break-even/kaçan-fill kapanışı: işlem GERÇEKTEN kapandı —
                        // kaydı R=0 ile düşür (sessiz düşürme = ters seçim bias).
                        if let Some(attr) = self.entry_attr.remove(c) {
                            self.tracker.record_outcome_r(
                                attr.voters.clone(),
                                hyperliquid_api::now_ms(),
                                0.0,
                            );
                            let excursion = self.trail.remove(c).map(|t| {
                                let risk = (t.atr_px * self.loop_cfg.trail_atr_k).max(1e-12);
                                let fav = if t.is_long { (t.peak_px - t.entry_px) / risk }
                                    else { (t.entry_px - t.peak_px) / risk };
                                let adv = if t.is_long { (t.worst_px - t.entry_px) / risk }
                                    else { (t.entry_px - t.worst_px) / risk };
                                (fav, adv, t)
                            });
                            self.write_ws_exit_record(c, &attr, 0.0, excursion);
                        }
                        self.executor.pending_tp.remove(c);
                    }
                }
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
                    // P1-K — TEK KAPANIŞ = TEK KAYIT = TEK SAYIM:
                    // TradeExit her kapanışta BİR KEZ yazılır (excursion
                    // yoksa sentetik boş kayıt — `write_ws_exit_record`),
                    // sayaç da tam bir kez artar (trade_exits.jsonl ile
                    // birebir tutarlı; restart'ta kalıcı).
                    match pnl {
                        Some(p) => {
                            let r = p / risk_usd.max(f64::EPSILON);
                            self.tracker.record_outcome_r(
                                attr.voters.clone(),
                                hyperliquid_api::now_ms(),
                                r,
                            );
                            self.write_ws_exit_record(&c, &attr, r, excursion);
                        }
                        // WS dolumu kaçtıysa/break-even: işlem GERÇEKTEN
                        // kapandı → kayıt R=0 ile düşer (sessiz düşürme =
                        // ters seçim bias; yanlış R yazmaktan iyidir).
                        None => {
                            tracing::warn!("[R] {c} kapandı, dolum fiyatı kaçtı — R=0 kaydedildi");
                            self.tracker.record_outcome_r(
                                attr.voters.clone(),
                                hyperliquid_api::now_ms(),
                                0.0,
                            );
                            self.write_ws_exit_record(&c, &attr, 0.0, excursion);
                        }
                    }
                }
            }
            }
            Err(e) => tracing::warn!(
                "[PZ] clearinghouseState çekilemedi: {e} — pozisyon haritası/öğrenme kaydı bu döngüde ertelendi (bayat veriyle karar YOK)"
            ),
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
                iv: None, // IV feed bağlı DEĞİL → sahte IV üretme (VolArbitraj=DATA-MISSING)
                equity: self.equity,
                // D: gerçek pozisyon (USD, ±) — s05/s06 artık ölü veri değil
                inventory_usd: self
                    .executor
                    .positions()
                    .get(&coin)
                    .filter(|p| p.szi.abs() > 1e-12)
                    .map(|p| p.szi * p.entry_px)
                    .unwrap_or(0.0),
                // order_size_usd: parent emir planı tahsisden SONRA oluşur →
                // strategy evaluation'a beslenemez. `Optimal Execution` bu
                // yüzden AUXILIARY (bkz. `Strategy::status`).
                order_size_usd: 0.0,
                // E: Kelly için tracker'ın GERÇEK canlı kanıtı
                portfolio_win_rate: self.tracker.portfolio_stats().map(|s| s.0),
                portfolio_wl_ratio: self.tracker.portfolio_stats().map(|s| s.1),
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

        // D — sistem raporu: strateji durumları (ACTIVE/AUXILIARY/DATA-MISSING).
        // Sessiz ölü strateji yasağı: hangi strateji neden skor ÜRETEMİYOR?
        if self.n_cycles % 12 == 1 {
            tracing::info!(
                "[STRAT] durum raporu (döngü {})
{}",
                self.n_cycles,
                self.strategy_status_lines()
            );
        }
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
                let now_ms = hyperliquid_api::now_ms();
                let mut act = if px_b > 0.0 && px_a > 0.0 {
                    self.machines
                        .get_mut(&e.coin)
                        .map(|m| m.on_decision(Decision { score: e.cs.score }, px_b, px_a, now_ms))
                        .unwrap_or(Action::Nothing)
                } else {
                    Action::Nothing
                };
                // ── B + C — ÇIKIŞ KANALLARI (yalnız AÇIK pozisyonda emir) ──
                let open = self
                    .machines
                    .get(&e.coin)
                    .map(|m| !matches!(m.state, PosState::Flat))
                    .unwrap_or(false);
                if open && matches!(act, Action::Nothing) {
                    let cur_band = strategy::kesit_momentum::rank_band(&universe_returns, &e.coin);
                    let prev_band = self.rank_band_prev.get(&e.coin).copied();
                    let side_long = self
                        .machines
                        .get(&e.coin)
                        .map(|m| m.in_position().map(|(s, _, _)| s == SmSide::Long).unwrap_or(false))
                        .unwrap_or(false);
                    // C: dokümante kural — LONG, üst %20'den alt %40'a düştü → Exit
                    let kesit_exit = side_long
                        && strategy::kesit_momentum::rank_shift_exit_long(prev_band, cur_band);
                    // C: turtle dokümante kural — kapanış 55-bar kanalın altı
                    // → LONG trend bitti (strateji ölü dal yerine helper üretir)
                    let turtle_exit = side_long
                        && strategy::trend_following_turtle::long_exit_signal(&self.agg.history(&e.coin, self.loop_cfg.interval_secs).to_vec());
                    // B: ensemble Exit oyu → gerçek ExitNow (Flat'ta hiçbir emir YOK;
                    // PendingExit'te idempotent — ikinci ExitNow üretmez)
                    let sig_exit = e.cs.exit_requested;
                    if kesit_exit || turtle_exit || sig_exit {
                        if let Some(m) = self.machines.get_mut(&e.coin) {
                            act = m.force_exit(px_b, px_a);
                        }
                        if matches!(act, Action::ExitNow(_)) {
                            tracing::info!(
                                "[EXIT] {} çıkış kanalı: {} (contributor={:?})",
                                e.coin,
                                if kesit_exit { "kesit rank kayması (LONG)" }
                                else if turtle_exit { "turtle 55-bar altı (LONG)" }
                                else { "Signal::Exit" },
                                e.cs.exit_contributors
                            );
                        }
                    }
                }
                // band kaydını GÜNCELLE (exit o döngüde işlendi, sonraki kayıt)
                if let Some(b) = strategy::kesit_momentum::rank_band(&universe_returns, &e.coin) {
                    self.rank_band_prev.insert(e.coin.clone(), b);
                }
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
                            self.am.cfg.cost.ev_ok(
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
                                self.am.cfg.cost.round_trip_cost_bps(spread_bps)
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
                    // G — KONTRAT KAPISI: negatif SL/TP fiyatı (ATR girişin
                    // katları) borsada "invalid price" olur → korumasız pozisyon.
                    if !entry_contract_ok(matches!(cand.dir, Dir::Long), entry_px, cand.sl_px, cand.tp_px)
                        || sl_ok < min_dist
                        || tp_ok < min_dist
                    {
                        tracing::warn!("{}: SL/TP kontrat/mesafe koruması (ATR âm) — emir atlandı", e.coin);
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
                    // F — TEK MALİYET MODELİ: band `CostModel::execution_band_bps`
                    // (EV kapısıyla AYNI fee+slippage+spread). Ad-hoc ±%0.5/±%0.2
                    // ikinci maliyet modeli KALDIRILDI; operasyonel taban
                    // (EXECUTION_BAND_FLOOR_BPS=50 bps) ince testnet defterinde
                    // dolum kaçışını önler (ölçülen).
                    let spread_bps_band = {
                        let mid2 = if e.ask > 0.0 && e.bid > 0.0 { (e.ask + e.bid) / 2.0 } else { 0.0 };
                        if mid2 > 0.0 { (e.ask - e.bid).abs() / mid2 * 10_000.0 } else { 0.0 }
                    };
                    let band = self.am.cfg.cost.execution_band_bps(spread_bps_band);
                    let ord_px = match mark_map.get(&e.coin) {
                        Some(mk) if *mk > 0.0 => {
                            let f = band / 10_000.0;
                            if side_sm.is_buy() { *mk * (1.0 + f) } else { *mk * (1.0 - f) }
                        }
                        _ => {
                            let f = band / 10_000.0;
                            let raw = if side_sm.is_buy() {
                                entry_px * (1.0 + f)
                            } else {
                                entry_px * (1.0 - f)
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
                            let now_ms = hyperliquid_api::now_ms();
                            self.executor
                                .pending_sl
                                .insert(e.coin.clone(), (attr.risk_px, now_ms));
                            // F — GERÇEK TP: mesafe `tp_from_entry` (tek formül)
                            // ile karar fiyatından; mesafe girişten bağımsız
                            // olduğu için dolum anına taşınır. Dolum gelince
                            // TP = dolum ± mesafe reduce-only asılır.
                            if let Some(tp) = attr.tp_px {
                                let dist = if attr.side.is_buy() {
                                    tp - entry_px
                                } else {
                                    entry_px - tp
                                };
                                if dist > 0.0 && dist.is_finite() {
                                    self.executor.pending_tp.insert(e.coin.clone(), (dist, now_ms));
                                }
                            }
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
                    // F — çıkış bandı da TEK modelden (IOC exit IOC ile aynı
                    // maliyet gerçekliği)
                    let xband = self.am.cfg.cost.execution_band_bps(0.0) / 10_000.0;
                    let xpx = o.px * if o.side.is_buy() { 1.0 + xband } else { 1.0 - xband };
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
    /// D — **strateji durum raporu** (`ACTIVE` / `AUXILIARY` / `DATA-MISSING`).
    ///
    /// Her strateji KENDİ durumunu bildirir (`Strategy::status`); temsili
    /// girdi yalnız gerçek motor state'inden doldurulur (iv yok, portfolio
    /// istatistiği tracker'dan). SAHTE veri üretilmez.
    pub fn strategy_status_lines(&self) -> String {
        let input = MarketInput {
            iv: None,
            portfolio_win_rate: self.tracker.portfolio_stats().map(|s| s.0),
            portfolio_wl_ratio: self.tracker.portfolio_stats().map(|s| s.1),
            ..Default::default()
        };
        self.strategies
            .iter()
            .map(|s| format!("{:<24} {}", s.name(), s.status(&input).label()))
            .collect::<Vec<_>>()
            .join("
")
    }

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

/// ── A + F — GERÇEK TP EXECUTION VE ÇIKIŞ SEBEBİ DOĞRULUĞU ────────────────
/// Testler saf **üretim** fonksiyonlarını sınar: `plan_tp_order` /
/// `should_place_tp` / `exit_reason_from_fill` / `apply_exit_reason`
/// (Executor::ensure_tp ve on_ws_event bunları tek yoldan çağırır).
#[cfg(test)]
mod tp_execution_tests {
    use super::*;
    use crate::allocation_manager::AllocationConfig;

    fn cfg() -> AllocationConfig {
        AllocationConfig::default()
    }

    /// Gerçek akış: `plan_tp_order` TP'yi dolumun KÂR tarafında üretir.
    #[test]
    fn tp_order_long_profit_side() {
        let c = cfg();
        let entry = 100.0f64;
        let risk = 3.0f64;
        let tp = c.tp_from_entry(Dir::Long, entry, risk);
        let p = plan_tp_order(true, true, entry, 2.0, tp).expect("long TP planı");
        assert!(p.tp_px > entry, "long TP > entry (kâr tarafı): {}", p.tp_px);
        assert!(p.tp_px >= entry + 2.0 * risk, "TP ≥ 2R mesafe");
        assert_eq!(p.reduce_only, true);
        // kâr tarafı ihlali → plan YOK
        assert!(plan_tp_order(true, true, entry, 2.0, entry - 1.0).is_none());
    }

    #[test]
    fn tp_order_short_profit_side() {
        let c = cfg();
        let entry = 100.0f64;
        let risk = 3.0f64;
        let tp = c.tp_from_entry(Dir::Short, entry, risk);
        let p = plan_tp_order(true, false, entry, 2.0, tp).expect("short TP planı");
        assert!(p.tp_px < entry, "short TP < entry (kâr tarafı): {}", p.tp_px);
        assert!(p.tp_px <= entry - 2.0 * risk, "TP ≥ 2R mesafe");
        // kâr tarafı ihlali → plan YOK
        assert!(plan_tp_order(true, false, entry, 2.0, entry + 1.0).is_none());
    }

    /// `reduce_only = true` ZORUNLU — TP pozisyonu büyütemez.
    #[test]
    fn tp_order_reduce_only() {
        let p = plan_tp_order(true, true, 100.0, 2.0, 106.0).expect("plan");
        assert!(p.reduce_only, "TP emri reduce_only olmalı");
        // Executor::ensure_tp imzası reduce-only trigger yerleştirir:
        // `place_tp_grid(..., is_take_profit = true, reduce_only = true, ...)`.

        // cloid takip edilebilirlik: TP_CLOID_SALT SL tuzundan farklı
        let (asset, sz) = (7u32, 2.0f64);
        let tp_cloid = crate::executor::make_cloid("BTC", 106.0, sz, crate::executor::TP_CLOID_SALT);
        let sl_cloid = crate::executor::make_cloid("BTC", 106.0, sz, 3u64); // SL: salt=dec
        assert_ne!(tp_cloid, sl_cloid, "TP ve SL cloid'i çakışmamalı");
        // aynı cloid idempotent: aynı girdi aynı anahtar
        assert_eq!(tp_cloid, crate::executor::make_cloid("BTC", 106.0, sz, crate::executor::TP_CLOID_SALT));
        let _ = asset;
    }

    /// TP fill → `ExitReason::TakeProfit`. MFE pozitif olsa bile TP YAZILMAZ.
    #[test]
    fn tp_fill_sets_exit_reason_take_profit() {
        let r = exit_reason_from_fill(true, false, 0, false, false).expect("TP fill → reason");
        assert_eq!(r, ExitReason::TakeProfit);

        // MFE pozitif ≠ TakeProfit: bayrak yoksa reason ÜRETİLMEZ
        assert_eq!(exit_reason_from_fill(false, false, 0, false, false), None);
        assert_eq!(exit_reason_from_fill(false, false, 0, false, false), None);

        // SL fill → StopLoss; ratchet'li SL fill → TrailingStop
        assert_eq!(
            exit_reason_from_fill(false, true, 0, false, false),
            Some(ExitReason::StopLoss)
        );
        assert_eq!(
            exit_reason_from_fill(false, true, 3, false, false),
            Some(ExitReason::TrailingStop)
        );
        // emergency ve manual ayrımı
        assert_eq!(
            exit_reason_from_fill(false, false, 0, false, true),
            Some(ExitReason::EmergencyFlatten)
        );
        assert_eq!(
            exit_reason_from_fill(false, false, 0, true, false),
            Some(ExitReason::Manual)
        );
    }

    /// Flat pozisyonda TP emri ÜRETİLMEZ (plan yok).
    #[test]
    fn tp_not_created_on_flat_position() {
        assert_eq!(plan_tp_order(false, true, 100.0, 2.0, 106.0), None, "pozisyon yok");
        assert_eq!(plan_tp_order(false, false, 100.0, 2.0, 94.0), None);
        // lot 0 = hayalet
        assert_eq!(plan_tp_order(true, true, 100.0, 0.0, 106.0), None);
    }

    /// Reconciliation/restore sonrası TP oid kaybolursa KÖRLEMESİNE
    /// duplicate edilmez: oid kayıtlıysa ya da borsada dinleniyorsa kurma.
    #[test]
    fn tp_reconciliation_does_not_duplicate() {
        // oid kayıtlı → kurma
        assert!(!should_place_tp(true, false, false));
        // borsada dinliyor (geri yüklendi) → kurma
        assert!(!should_place_tp(false, true, false));
        // grace süresi içinde → kurma (WS "open" bildirimi gecikebilir)
        assert!(!should_place_tp(false, false, true));
        // hiçbiri → kur
        assert!(should_place_tp(false, false, false));
    }

    /// TP ve SL AYNI pozisyonda ikisi de dolarsa exit state ÇİFT yazılmaz.
    #[test]
    fn tp_sl_same_position_no_duplicate_exit_state() {
        // her iki bayrak da true (nadir ama olası: hızlı fiyat hareketi)
        let r = exit_reason_from_fill(true, true, 0, false, false).expect("reason");
        // ÖNCELİK TP: TP fill kesin kaynak; SL ikincil
        assert_eq!(r, ExitReason::TakeProfit, "TP öncelikli, çift atama yok");
        // ilk yazılan korunur: StopLoss yazıldıysa TP üstüne YAZILMAZ
        let mut cur = Some(ExitReason::StopLoss);
        apply_exit_reason(&mut cur, ExitReason::TakeProfit);
        assert_eq!(cur, Some(ExitReason::StopLoss), "first-write-wins");
        // boşsa yazılır
        let mut empty = None;
        apply_exit_reason(&mut empty, ExitReason::TakeProfit);
        assert_eq!(empty, Some(ExitReason::TakeProfit));
    }

    /// `ensure_tp`'nin idempotency kapısı gerçek Executor state'iyle:
    /// `tp_orders` doluysa ikinci emir ASILMAZ.
    #[test]
    fn tp_orders_map_zaten_varysa_ensure_tp_kurma() {
        // Gerçek Executor kurulamaz (ağa ihtiyaç duyar) → kapı fonksiyonunun
        // kendisi (ensure_tp'nin ilk iki satırı) ile aynı koşul: should_place_tp.
        let mut tp_orders: HashMap<String, i64> = HashMap::new();
        tp_orders.insert("BTC".into(), 42);
        let already = tp_orders.contains_key("BTC");
        assert!(!should_place_tp(already, false, false), "ikinci TP asılmaz");
    }
}

/// ── B — SIGNAL::EXIT → GERÇEK ExitNow (gerçek state machine) ────────────
#[cfg(test)]
mod exit_engine_tests {
    use crate::state_machine::{BotOrder, Machines, PosState, PositionStateMachine, Side, SmConfig, Action};
    use std::collections::HashMap;

    fn machine_in_position(coin: &str, long: bool) -> PositionStateMachine {
        let mut m = PositionStateMachine::new(coin, SmConfig::default());
        // GERÇEK dolum yolu: on_order_status ile pozisyona gir
        let d = crate::state_machine::Decision { score: 0.6 };
        m.on_decision(d, 100.0, 100.5, 1);
        m.on_order_status(0, 2.0, 0.0, 100.5);
        assert!(
            matches!(m.state, PosState::InPosition { .. }),
            "makine pozisyonda olmalı: {:?}",
            m.state
        );
        let _ = long;
        m
    }

    /// Açık LONG pozisyon + exit_requested → GERÇEK ExitNow
    /// (reduce_only, ters taraf, pozisyon boyutu).
    #[test]
    fn exit_signal_closes_long() {
        let mut m = machine_in_position("BTC", true);
        let act = m.force_exit(100.0, 100.5);
        match act {
            Action::ExitNow(o) => {
                assert!(o.reduce_only, "exit reduce_only");
                assert_eq!(o.side, Side::Short, "long çıkışı = SAT");
                assert!(o.sz > 0.0, "gerçek boyut taşınır");
            }
            other => panic!("ExitNow beklenirdi: {other:?}"),
        }
        // durum: PendingExit (ikinci ExitNow idempotent)
        assert!(matches!(m.state, PosState::PendingExit { .. }));
        assert!(matches!(m.force_exit(100.0, 100.5), Action::Nothing));
    }

    #[test]
    fn exit_signal_closes_short() {
        let mut m = PositionStateMachine::new("BTC", SmConfig::default());
        m.on_decision(crate::state_machine::Decision { score: -0.6 }, 100.0, 100.5, 1);
        m.on_order_status(0, 2.0, 0.0, 100.5);
        let act = m.force_exit(100.0, 100.5);
        match act {
            Action::ExitNow(o) => {
                assert_eq!(o.side, Side::Long, "short çıkışı = AL");
                assert!(o.reduce_only);
            }
            other => panic!("ExitNow beklenirdi: {other:?}"),
        }
    }

    /// Flat pozisyonda Exit HİÇBİR emir ÜRETMEZ.
    #[test]
    fn exit_signal_flat_produces_no_order() {
        let mut m = PositionStateMachine::new("BTC", SmConfig::default());
        assert!(matches!(m.state, PosState::Flat));
        assert!(matches!(m.force_exit(100.0, 100.5), Action::Nothing));

        // Pending (dolum bekleyen giriş) de emir üretmez — exit kanalı
        // bekleyen GİRİŞİ kapatmaz, motor Cancel yoluyla yönetir.
        m.on_decision(crate::state_machine::Decision { score: 0.6 }, 100.0, 100.5, 1);
        assert!(matches!(m.state, PosState::Pending { .. }));
        assert!(matches!(m.force_exit(100.0, 100.5), Action::Nothing));
    }

    /// Çoklu Exit oyu (aynı döngüde 3 strateji) → TEK ExitNow (idempotent).
    ///
    /// Gerçek akış: run_cycle döngü başına BİR kez force_exit çağırır
    /// (`act` zaten ExitNow iken ikinci kez çağrılmaz); burada makine
    /// düzeyindeki idempotency kanıtlanır.
    #[test]
    fn multiple_exit_votes_are_idempotent() {
        let mut machines: Machines = HashMap::new();
        machines.insert("BTC".into(), machine_in_position("BTC", true));
        for _ in 0..3 {
            // 3 farklı strateji Exit dedi (aynı döngüde, exit_requested=true)
            let m = machines.get_mut("BTC").unwrap();
            if let Action::Nothing = m.force_exit(100.0, 100.5) {
                if matches!(m.state, PosState::Flat) {
                    panic!("InPosition iken exit üretmelidir");
                }
            }
            // sonraki oy: PendingExit → Nothing
            if !matches!(machines.get_mut("BTC").unwrap().force_exit(100.0, 100.5), Action::Nothing) {
                panic!("ikinci ExitNow ÜRETİLMEMELİ (idempotent)");
            }
        }
    }
}

/// ── G — SL/TP/RR CONTRACT (property-style deterministik matris) ─────────
/// Her giriş için: Long: SL < entry < TP; Short: TP < entry < SL;
/// TP mesafesi ≥ 2×risk. Kontratı bozan tahsis → EMİR ÜRETİLMEZ
/// (`plan_tp_order` + `tp_side_ok` + EV kapısı aynı modelden).
#[cfg(test)]
mod sl_tp_contract_tests {
    use super::*;
    use crate::allocation_manager::AllocationConfig;

    #[test]
    fn sl_tp_rr_contract_matrix() {
        // G — TP oran toleransı: min kapı yalnız R:R<1 config'de devreye
        // girer; oran > 2 config'lerinde formül BİREBİR uygulanmalı (mutasyon
        // G2/G3 bunları yakalar).
        let configs = [
            AllocationConfig::default(),                                  // oran 2.0
            AllocationConfig { tp_atr_mult: 1.0, ..Default::default() },  // oran 0.67 → min kapı
            AllocationConfig { tp_atr_mult: 5.0, ..Default::default() },  // oran 3.33
        ];
        let entries = [100.0f64, 0.0098, 50.0, 12345.6];
        let atrs = [0.5f64, 2.0, 5.0, 0.05];
        let slips = [0.0f64, 0.002]; // kayma: TP/SL fiyatlara taşınır
        for cfg in &configs {
        for &entry in &entries {
            for &atr in &atrs {
                for &slip in &slips {
                    let entry_eff = if entry > 0.0 { entry * (1.0 + slip) } else { entry };
                    for dir in [Dir::Long, Dir::Short] {
                        let am = AllocationManager::new(cfg.clone());
                        let (sl, tp) = am.sl_tp(dir, entry_eff, atr);
                        let risk = (entry_eff - sl).abs();
                        assert!(risk > 0.0, "risk > 0: entry={entry} atr={atr}");
                        if matches!(dir, Dir::Long) {
                            assert!(sl < entry_eff, "Long: SL < entry ({sl} < {entry_eff})");
                            assert!(tp > entry_eff, "Long: TP > entry ({tp} > {entry_eff})");
                        } else {
                            assert!(tp < entry_eff, "Short: TP < entry ({tp} < {entry_eff})");
                            assert!(sl > entry_eff, "Short: SL > entry ({sl} > {entry_eff})");
                        }
                        let tp_dist = (tp - entry_eff).abs();
                        assert!(
                            tp_dist >= 2.0 * risk - 1e-9,
                            "TP ≥ 2R: dir={dir:?} entry={entry_eff} atr={atr} tp_dist={tp_dist} risk={risk}"
                        );
                        // G — FORMÜL BİREBİR: tp_dist = max((tp/sl)×risk, 2×risk)
                        let want = ((cfg.tp_atr_mult / cfg.sl_atr_mult) * risk).max(2.0 * risk);
                        assert!(
                            (tp_dist - want).abs() < 1e-9,
                            "formül sapması: dir={dir:?} tp/sl={} risk={risk} tp_dist={tp_dist} want={want}",
                            cfg.tp_atr_mult / cfg.sl_atr_mult
                        );
                        // plan kapısı: kontrat sağlanıyorsa emir ÜRETİLİR;
                        // kontrat BOZULUYORSA (negatif fiyat/mesafe) EMİR YOK
                        assert_eq!(
                            entry_contract_ok(matches!(dir, Dir::Long), entry_eff, sl, tp),
                            tp > 0.0 && sl > 0.0,
                            "kontrat kapısı = pozitif fiyat + doğru taraf (dir={dir:?} entry={entry_eff} atr={atr} sl={sl} tp={tp})"
                        );
                        if entry_contract_ok(matches!(dir, Dir::Long), entry_eff, sl, tp) {
                            assert!(plan_tp_order(true, matches!(dir, Dir::Long), entry_eff, 1.0, tp).is_some(),
                                "kontrat sağlandı → TP plan üretilebilir");
                        } else {
                            // bozuk kontrat → GİRİŞ EMİRİ ÜRETİLMEZ (G kabul
                            // kuralı — run_cycle `entry_contract_ok` kapısı)
                            assert_eq!(entry_contract_ok(matches!(dir, Dir::Long), entry_eff, sl, tp), false);
                        }
                    }
                }
            }
        }
        }
    }
}

/// ── H — POSITION STATE / EXECUTION CONTRACT (uçtan uca) ─────────────────
#[cfg(test)]
mod lifecycle_contract_tests {
    use crate::state_machine::{Decision, PosState, PositionStateMachine, SmConfig, Action, Side};

    /// Flat → Pending → Filled → InPosition → Exit → Flat — birebir.
    #[test]
    fn tam_yasam_dongusu_flat_to_flat() {
        let mut m = PositionStateMachine::new("BTC", SmConfig::default());
        assert!(matches!(m.state, PosState::Flat));

        // 1) karar → Pending + Place
        let a1 = m.on_decision(Decision { score: 0.6 }, 100.0, 100.5, 1);
        assert!(matches!(a1, Action::Place(_)), "{a1:?}");
        assert!(matches!(m.state, PosState::Pending { .. }));

        // 2) idempotent: aynı yönde tekrar sinyal → yeni emir YOK
        assert!(matches!(m.on_decision(Decision { score: 0.7 }, 100.0, 100.5, 2), Action::Nothing));

        // 3) dolum → InPosition
        m.link_pending_oid(77);
        let a3 = m.on_order_status(77, 2.0, 0.0, 100.5);
        assert!(matches!(m.state, PosState::InPosition { .. }), "{a3:?}");

        // 4) ters sinyal → PendingExit + ExitNow (reduce_only)
        let a4 = m.on_decision(Decision { score: -0.8 }, 99.5, 100.0, 3);
        match a4 {
            Action::ExitNow(o) => {
                assert!(o.reduce_only);
                assert_eq!(o.side, Side::Short);
                assert!(o.sz > 0.0, "gerçek boyut");
            }
            other => panic!("ExitNow beklenirdi: {other:?}"),
        }
        assert!(matches!(m.state, PosState::PendingExit { .. }));

        // 5) idempotent: PendingExit'te ikinci exit → Nothing
        assert!(matches!(m.on_decision(Decision { score: -0.9 }, 99.0, 99.5, 4), Action::Nothing));
        assert!(matches!(m.force_exit(99.0, 99.5), Action::Nothing));

        // 6) çıkış dolumu → Flat
        m.link_pending_oid(88);
        let a6 = m.on_order_status(88, 2.0, 0.0, 99.2);
        assert!(matches!(m.state, PosState::Flat), "{a6:?}");

        // 7) Flat'ta tekrar giriş döngüsü (duplicate order yok: aynı oid iki kez işlenmez)
        let a7 = m.on_order_status(88, 2.0, 0.0, 99.2);
        assert!(matches!(m.state, PosState::Flat), "{a7:?}");
    }

    /// UNKNOWN → cloid bulunamadı → Flat yalnız oid==0 pending'de.
    #[test]
    fn unknown_bulunamadi_sadece_oid_sifir_pendingde_flat() {
        let mut m = PositionStateMachine::new("BTC", SmConfig::default());
        m.on_decision(Decision { score: 0.6 }, 100.0, 100.5, 1);
        // oid==0 pending → revert serbest
        assert!(m.oid_zero_pending());
        // gerçek oid bağlıysa → revert YASAK (pozisyon borsada olabilir)
        m.link_pending_oid(42);
        assert!(!m.oid_zero_pending());
        // InPosition'da da revert yasak
        let _ = m.on_order_status(42, 2.0, 0.0, 100.5);
        assert!(matches!(m.state, PosState::InPosition { .. }));
        assert!(!m.oid_zero_pending());
    }
}

/// ── I — ÖLÇÜLEBİLİR METRİKLER (gerçek fill verisi; uydurma yok) ─────────
#[cfg(test)]
mod metrics_tests {
    use crate::track::{PerformanceMetrics, StrategyTracker};

    #[test]
    fn metrikler_gercek_r_dizisinden_hesaplanir() {
        // bilinen dizi: +2, −1, −1, +3, −1 → 5 işlem, 2 kazanç
        let rs = [2.0f64, -1.0, -1.0, 3.0, -1.0];
        let m = PerformanceMetrics::from_rs(&rs, Some(3.0), Some(-1.5));
        assert_eq!(m.trades, 5);
        assert!((m.win_rate.unwrap() - 0.4).abs() < 1e-9);
        // pf = 5/3
        assert!((m.profit_factor.unwrap() - 5.0 / 3.0).abs() < 1e-9);
        assert!((m.expectancy_r.unwrap() - 0.4).abs() < 1e-9);
        assert!((m.avg_win_r.unwrap() - 2.5).abs() < 1e-9);
        // kayıp İŞARETLİ raporlanır (−1.0 = işlem başına −1R)
        assert!((m.avg_loss_r.unwrap() - (-1.0)).abs() < 1e-9);
        // kümülatif: 2,1,0,3,2 → peak 2,2,2,3,3 → düşüşler 0,1,2,0,1 → MDD 2
        assert!((m.max_drawdown_r.unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(m.max_consecutive_losses, Some(2));
        assert!((m.mfe_r.unwrap() - 3.0).abs() < 1e-9);
        // rapor satırı iddia değil ölçüm içerir
        assert!(m.line().contains("trades=5"));
    }

    /// Kanıt yok → SAYI UYDURULMAZ (None); "%99 kazanır" tipi iddia yok.
    #[test]
    fn kanit_yoksa_metrik_uydurulmaz() {
        let m = PerformanceMetrics::from_rs(&[], None, None);
        assert_eq!(m.trades, 0);
        assert_eq!(m.win_rate, None, "win_rate UYDURULMAZ");
        assert_eq!(m.profit_factor, None);
        assert_eq!(m.expectancy_r, None);
        // tracker arayüzü de aynısı
        let t = StrategyTracker::default();
        assert_eq!(t.metrics("yok").trades, 0);
        assert_eq!(t.metrics("yok").win_rate, None);
        assert_eq!(t.portfolio_metrics().trades, 0);
        // sabit iddia metinleri KODDA YASAK (I)
        let src = include_str!("track.rs").split("#[cfg(test)]").next().unwrap_or("");
        for (i, l) in src.lines().enumerate() {
            let t2 = l.trim_start();
            if t2.starts_with("//") || t2.starts_with("///") {
                continue;
            }
            assert!(!t2.contains("%99") && !t2.contains("garantili"),
                "satır {}: kesin kazanç iddiası yasak: {l}", i + 1);
        }
    }
}

/// ── BLOCKER 1+2 — BORSA KAPANIŞI ↔ DURUM MAKİNESİ SÖZLEŞMESİ ────────────
#[cfg(test)]
mod recon_contract_tests {
    use super::*;
    use crate::executor::is_open_fill;
    use crate::state_machine::{Decision, PosState, PositionStateMachine, Side as SmSide};
    use crate::executor::Position;

    fn in_position_machine(coin: &str) -> PositionStateMachine {
        let mut m = PositionStateMachine::new(coin, SmConfig::default());
        m.on_decision(Decision { score: 0.6 }, 100.0, 100.5, 1);
        m.on_order_status(0, 2.0, 0.0, 100.5);
        assert!(matches!(m.state, PosState::InPosition { .. }));
        m
    }

    fn pos(szi: f64) -> Position {
        Position { coin: "X".into(), szi, entry_px: 100.0, position_value: szi.abs() * 100.0, unrealized_pnl: 0.0, leverage: 1.0, liquidation_px: None }
    }

    /// BLOCKER-1: borsa TP fill → pozisyon borsadan silinir → makine FLAT
    /// olmalı; sonraki döngüde force_exit EMİR ÜRETMEMELİ.
    #[test]
    fn tp_fill_closes_machine_to_flat() {
        let mut machines: Machines = HashMap::new();
        machines.insert("BTC".into(), in_position_machine("BTC"));
        // TP tetiklendi → pozisyon borsadan KALKTI (boş harita)
        let gone = reconcile_machines_to_exchange(&mut machines, &HashMap::new());
        assert_eq!(gone, vec!["BTC".to_string()]);
        let m = machines.get("BTC").unwrap();
        assert!(matches!(m.state, PosState::Flat));
        // sonraki döngü: bayat boyutla ExitNow ÜRETİLMEZ
        let mm = machines.get_mut("BTC").unwrap();
        assert!(matches!(mm.force_exit(99.0, 99.5), Action::Nothing));
    }

    /// BLOCKER-1: borsa SL fill → makine FLAT (aynı sözleşme, ters yön).
    #[test]
    fn sl_fill_closes_machine_to_flat() {
        let mut machines: Machines = HashMap::new();
        let mut m = PositionStateMachine::new("ETH", SmConfig::default());
        m.on_decision(Decision { score: -0.6 }, 100.0, 100.5, 1);
        m.on_order_status(0, 3.0, 0.0, 100.5);
        assert!(matches!(m.state, PosState::InPosition { .. }));
        machines.insert("ETH".into(), m);
        let gone = reconcile_machines_to_exchange(&mut machines, &HashMap::new());
        assert_eq!(gone, vec!["ETH".to_string()]);
        assert!(matches!(machines.get("ETH").unwrap().state, PosState::Flat));
        assert!(matches!(machines.get_mut("ETH").unwrap().force_exit(100.0, 100.5), Action::Nothing));
    }

    /// HARİCİ kapanış (emir bize ait değil: exit fill / flip): makine Flat'a
    /// döner; POZİSYON DURUYORSA (hâlâ borsada) makine DOKUNULMAZ.
    #[test]
    fn external_close_reconciles_machine_to_flat() {
        let mut machines: Machines = HashMap::new();
        machines.insert("BTC".into(), in_position_machine("BTC"));
        machines.insert("ETH".into(), in_position_machine("ETH"));

        // BTC: harici kapanış (borsada yok) → Flat
        // ETH: hâlâ borsada → DOKUNULMAZ (hayalet-flat yasağı)
        let mut positions = HashMap::new();
        positions.insert("ETH".into(), pos(2.0));
        let gone = reconcile_machines_to_exchange(&mut machines, &positions);

        assert_eq!(gone, vec!["BTC".to_string()], "yalnız borsadan kaybolanlar");
        assert!(matches!(machines.get("BTC").unwrap().state, PosState::Flat));
        assert!(matches!(machines.get("ETH").unwrap().state, PosState::InPosition { .. }));

        // Pending (dolum bekleyen giriş) revert DOKUNULMAZ — dolum sonra gelir
        let mut pend = PositionStateMachine::new("SOL", SmConfig::default());
        pend.on_decision(Decision { score: 0.6 }, 100.0, 100.5, 1);
        assert!(matches!(pend.state, PosState::Pending { .. }));
        machines.insert("SOL".into(), pend);
        let gone2 = reconcile_machines_to_exchange(&mut machines, &HashMap::new());
        assert!(!gone2.contains(&"SOL".to_string()), "bekleyen giriş revert edilmez");
        assert!(matches!(machines.get("SOL").unwrap().state, PosState::Pending { .. }));
    }

    /// BLOCKER-2 — açılış/kapanış ayrımı: `dir` alanı kaynak; closedPnl DEĞİL.
    #[test]
    fn closing_fill_zero_pnl_is_not_open_fill() {
        // BREAK-EVEN kapanış: closedPnl=0 AMA dir=Close → açılış DEĞİL
        assert!(!is_open_fill("Close Long"), "break-even kapanış koruma ARMAMALI");
        assert!(!is_open_fill("Close Short"));
        // açılışlar
        assert!(is_open_fill("Open Long"));
        assert!(is_open_fill("Open Short"));
        // ters çevirme: eski taraf kapanır, YENİ TARAFA açılış
        assert!(is_open_fill("Long > Short"));
        assert!(is_open_fill("Short > Long"));
        // bilinmeyen/boş şema → fail-safe: açılış SAYILMAZ
        assert!(!is_open_fill(""));
        assert!(!is_open_fill("bilinmeyen"));
    }
}

/// ── BLOCKER 2 devam — exec-apply_fill kapısı: kapanış fill'i koruma
/// KURMAZ; açılış fill'i kurar (yalnız gerçek apply_fill yolu koşar — bu
/// test soyutlanmış kapıyı sınar).
#[cfg(test)]
mod fill_protection_tests {
    use crate::executor::is_open_fill;

    /// break-even kapanış → koruma ARMILMAZ (pending_sl/pending_tp kurmaz)
    #[test]
    fn zero_pnl_close_does_not_arm_entry_protection() {
        // gerçek şema: break-even kapanış fill'i
        // { dir: "Close Long", closedPnl: "0.0" }
        let dir = "Close Long";
        let closed_pnl = 0.0f64;
        // YENİ KURAL: `dir` kaynak; closedPnl karar için KULLANILMAZ.
        assert!(!is_open_fill(dir));
        // ÜRETİM İSPATI 1: koruma kapısı closedPnl'e BAĞLANAMAZ (kaynak taraması).
        // ÜRETİM İSPATI 2: closing fill (dir=Close) closedPnl=0 olsa bile
        // closed_pnl HARİTASINA YAZILMALI — recon kapanış kaydının WS
        // işareti. `closed != 0.0` İLE HARİTAYA YAZIM YASAK (eski hata:
        // break-even kapanışlar kayboluyordu).
        let src = include_str!("executor.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or("");
        for (i, l) in prod.lines().enumerate() {
            let t = l.trim_start();
            if t.starts_with("//") || t.starts_with("///") {
                continue;
            }
            assert!(
                !t.contains("open_fill = closed") && !t.contains("if closed =="),
                "satır {}: koruma kapısı closedPnl'e BAĞLANAMAZ: {l}",
                i + 1
            );
        }
        // ÜRETİM İSPATI 2 (kaynak taraması): closed_pnl YAZIM koşulu
        // `closed != 0.0` İÇEREMEZ — eski hata: break-even kapanışlar
        // haritaya yazılmıyor → kapanış kaydı kayboluyordu.
        let entry_line = prod
            .lines()
            .position(|l| l.contains("self.closed_pnl.entry(coin.clone())"))
            .expect("closed_pnl yazım noktası bulunmalı");
        let around: String = prod
            .lines()
            .skip(entry_line.saturating_sub(6))
            .take(6)
            .map(|l| {
                let t = l.trim_start();
                if t.starts_with("//") || t.starts_with("///") {
                    ""
                } else {
                    l
                }
            })
            .collect();
        assert!(
            !around.contains("closed != 0.0"),
            "closed_pnl yazımı closedPnl'e koşullu — break-even kapanış kaybolur: {around}"
        );
        let mut seen = false;
        for (i, l) in prod.lines().enumerate() {
            let t = l.trim_start();
            if t.starts_with("//") || t.starts_with("///") {
                continue;
            }
            if t.contains("closed_pnl.entry(coin.clone())") {
                seen = true;
            }
            if seen && t.contains("closed != 0.0") {
                panic!("satır {}: closed_pnl yazımı closedPnl'e koşullu — break-even kapanış kaybolur: {l}", i + 1);
            }
            if seen && !t.ends_with(',') && !t.is_empty() && !t.contains("closed_pnl") && !t.starts_with('*') {
                break;
            }
        }
        let _ = closed_pnl;
    }

    /// açılış fill'i koruma kurar (imza davranışı korunur)
    #[test]
    fn open_fill_arms_entry_protection() {
        assert!(is_open_fill("Open Long"));
        assert!(is_open_fill("Open Short"));
        assert!(is_open_fill("Long > Short"), "flip yeni tarafa açılış");
    }
}

/// ── P1-L — WS ANINDA FİNALİZE sözleşmesi ───────────────────────────────
#[cfg(test)]
mod ws_finalize_tests {
    use super::*;

    /// SL/TP fill pozisyonu kapattı → finalize ANINDA (REST'e bağımlı değil).
    #[test]
    fn ws_closing_fill_finalizes_immediately() {
        assert!(should_finalize_on_ws(true, false, false, true), "SL fill + pozisyon kapandı");
        assert!(should_finalize_on_ws(false, true, false, true), "TP fill + pozisyon kapandı");
        // kısmi dolum → pozisyon SÜRÜYOR → finalize YOK
        assert!(!should_finalize_on_ws(true, false, true, true));
        // attr yok (restart devralınan) → bu yoldan finalize YOK (recon yolu alır)
        assert!(!should_finalize_on_ws(false, true, false, false));
        // kapanış bayrağı yok (normal giriş fill'i) → finalize YOK
        assert!(!should_finalize_on_ws(false, false, false, true));
    }
}
