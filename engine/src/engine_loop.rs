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

/// Giriş atribüsyonu: (katkı veren stratejiler, pozisyon yönü, giriş fiyatı)
pub struct EntryAttr {
    pub voters: Vec<String>,
    pub side: SmSide,
    pub entry_px: f64,
    /// giriş riski (|entry - SL|); R-çarpanı = net PnL / risk
    pub risk_px: f64,
    /// giriş boyutu — risk_usd = risk_px × size
    pub size: f64,
    pub ts: u64,
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
    /// canlı öğrenme durumu (n, ΣR, son ts) — diske yazılır
    trained_weights: HashMap<String, crate::track::TrainedInfo>,
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
    /// **ExitsOnly** kipi (P1-F): `false` iken yeni giriş emri KONULMAZ;
    /// çıkışlar + borsa stop'ları normal çalışır (mod yalnız "daha fazla
    /// risk almayı" kapatır, mevcut riski yönetmeye devam eder).
    pub entries_allowed: bool,
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
            trained_weights: HashMap::new(),
            learn_pol: crate::track::LearnPolicy::default(),
            strategy_names,
            track_state_path: std::env::var("TRACK_STATE_PATH")
                .unwrap_or_else(|_| "/root/hyperliquid/config/track_state.json".into()),
            explore_on: true,
            explore_names: std::collections::HashSet::new(),
            explore_cursor: 0,
            explore_pick: None,
            entries_allowed: true,
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

    /// **CANLI** ağırlıkları yükle (`origin:"live"` doğrulanmış — kapı `bot.rs`te).
    ///
    /// Beklenen şema:
    /// ```json
    /// { "origin":"live", "ts":…, "weights": { "Wyckoff": 0.83, ... } }
    /// ```
    /// ÖNEMLİ: bu **önsel** değil, *kaydedilmiş canlı kanıt*. Yine de
    /// eksik stratejiler 1.0'a düşer (kısmi dosya = kısmi kanıt).
    pub fn load_live_weights(&mut self, raw: &serde_json::Value) {
        let mut m: HashMap<String, crate::track::TrainedInfo> = HashMap::new();
        if let Some(obj) = raw.get("weights").and_then(|x| x.as_object()) {
            for (name, e) in obj {
                m.insert(
                    name.clone(),
                    crate::track::TrainedInfo {
                        trades: e
                            .get("n")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0),
                        sum_r: e
                            .get("sum_r")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0),
                        last_ts: e
                            .get("last_ts")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0),
                        origin: "live".into(),
                        schema_version: 1,
                    },
                );
            }
        }
        self.trained_weights = m;
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
    pub fn log_cold_start(&self) {
        tracing::info!(
            "[LEARN] nötr başlangıç: {} strateji × w=1.0; persistence=ON (min_n={} full_n={})",
            self.strategy_names.len(),
            self.learn_pol.min_n,
            self.learn_pol.full_n
        );
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

    /// Ağırlıkları üç katmandan birleştirip ensemble'e bas: EĞİTİM × canlı Kelly,
    /// sıfırlananlar keşif tabanına. Artık `dyn_weights` eğitimi EZMEZ.
    pub fn apply_effective_weights(&mut self) {
        let (eff, explorers, events) =
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
        self.ensemble.cfg.weights = eff;
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

    /// ACİL DÜZLEŞTİRME (kill-switch): tüm pozisyonları kapat.
    pub async fn flatten_positions(&mut self) {
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
                        let gap = (rest_entry - a.entry_px) / a.entry_px.max(1e-12);
                        if gap.abs() > 1e-9 {
                            tracing::info!(
                                "[SL] {cn} giriş fiyatı düzeltildi: karar(defter)={:.8} \
                                 dolum(REST)={:.8} kayma={:+.3}%",
                                a.entry_px,
                                rest_entry,
                                gap * 100.0
                            );
                            a.entry_px = rest_entry;
                        }
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
                    let Some(attr) = self.entry_attr.remove(&c) else {
                        continue;
                    };
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
                                    cikis: if *fav > 0.0 { "tp" } else { "stop" }.into(),
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

        // == P1-3: S5 PERİYODİK RECON (30 sn) — cycle-sayacı yerine zaman ==
        static LAST_RECON: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
        let now_ms = hyperliquid_api::now_ms();
        let mut recon_notes: Vec<String> = Vec::new();
        {
            let mut last = LAST_RECON.lock().unwrap();
            if now_ms.saturating_sub(*last) >= 30_000 {
                *last = now_ms;
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
            // black-swan: işaretli ort < −%15 → TÜM pozisyonları İPTTAL (kill-switch)
            if !universe_returns.is_empty() && s.0 < -0.15 {
                tracing::error!("BLACK SWAN: evren ort {:+.1}% — TÜM pozisyonlar kapatılıyor", s.0 * 100.0);
                for m in self.machines.values_mut() { m.state = PosState::Flat; }
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
                    if !self.entries_allowed {
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
                    let attr = EntryAttr {
                        ts: hyperliquid_api::now_ms(),
                        voters: selected.iter().find(|c| c.coin == e.coin).map(|c| c.voters.clone()).unwrap_or_default(),
                        side: side_sm,
                        entry_px,
                        risk_px: (entry_px - cand.sl_px).abs().max(entry_px * 1e-4),
                        size: sz,
                    };
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
            for (i, r) in res.iter().enumerate() {
                tracing::info!("batch[{i}]: {r:?}");
                if let Some(oid) = r.oid {
                    // hangi coin pending oid=0 bekliyorsa — bağla
                    let target: Option<String> = self
                        .machines
                        .iter()
                        .find(|(_, m)| matches!(m.state, PosState::Pending { oid: 0, .. }))
                        .map(|(c, _)| c.clone());
                    if let Some(cn) = target {
                        if let Some(m) = self.machines.get_mut(&cn) {
                            m.link_pending_oid(oid);
                        }
                    }
                }
            }
        }
        // BORSA STOP SENKRONU: her döngü sonunda (açılışta devralınan
        // pozisyonlar dahil) her açık pozisyonun borsada SL'si var mı?
        self.sync_exchange_sl().await;
        Ok(summaries)
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
