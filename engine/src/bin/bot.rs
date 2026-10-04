//! BOT — ana binary: NTP → ingestor WS → engine döngüsü.
//! ENV:
//!   ENGINE_COINS=BTC,ETH,SOL   (döngüde koşacak coinler; boş/"all" = tam evren)
//!   ENGINE_INTERVAL=3600       (MUM aralığı, sn — döngü periyodu DEĞİL)
//!   ENGINE_CYCLE_SECS=5        (motor döngü periyodu, sn)
//!   ENGINE_MIN_SCORE=0.30      (ensemble eşiği)
//!   ENGINE_TOP_N=3             (pozisyon tavanı)
//!   ENGINE_BUDGET_USD=300      (döngü bütçesi; tahsis ayrıca $50/coin ile sınırlı)
//!   ENGINE_HIST_BARS=4400      (indikatör ısınma penceresi)
//!   ENGINE_FLATTEN=1           (açılışta tüm pozisyonları kapat)
//!   ENGINE_TICK_CACHE=…        (öğrenilen tick hassasiyeti dosyası)
//!   ENGINE_EXPLORE=1           (veri-açlığı keşif kuyruğu açık)
//!   ENGINE_CRIT_THRESHOLD=5    (kill-switch: **karar-kritik** hata eşiği)
//!   TRAIL_ON=1 TRAIL_ATR_K=1.5 TRAIL_MIN_STEP=0.25   (trailing stop, §8.3.15)
//!
//! CANLI ÖĞRENME (§8.4 — backtest kaldırıldı):
//!   LEARN_MIN_N=10             (bu n altında ağırlık DONMUŞ 1.0)
//!   LEARN_FULL_N=30            (bu n üstü: tam kurallar + keşif kuyruğu dışı)
//!   TRACK_STATE_PATH=…         (tracker persistence, atomik 5 dk dump)
//!   ENSEMBLE_WEIGHTS_IN=…      (yalnız origin:"live" kabul edilir)
//!   ENSEMBLE_LIVE_WEIGHTS_OUT=…
//!   EV_TAKER_FEE_BPS=4.5 EV_SLIPPAGE_BPS=5.0 EV_MIN_DIST_BPS=75  (P0-D EV kapısı)
//!
//! Çalıştırma süresi yok — Ctrl+C ile kes.

use engine::engine_loop::{Engine, LoopConfig};
use engine::track::TradeExit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use utils::{logger, time};

// S4: WS kuyruk izleme için basit sarmalayıcı
struct MonitoredRx {
    rx: tokio::sync::mpsc::Receiver<hyperliquid_api::websocket::WsEvent>,
    pending: Arc<AtomicUsize>,
}
impl MonitoredRx {
    fn new(rx: tokio::sync::mpsc::Receiver<hyperliquid_api::websocket::WsEvent>, pending: Arc<AtomicUsize>) -> Self {
        Self { rx, pending }
    }
    async fn recv(&mut self) -> Option<hyperliquid_api::websocket::WsEvent> {
        let event = self.rx.recv().await;
        if event.is_some() {
            self.pending.fetch_sub(1, Ordering::Relaxed);
        }
        event
    }
    fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logger::init();
    if let Some(off) = time::sync(time::DEFAULT_SERVERS, std::time::Duration::from_secs(3)) {
        tracing::info!("NTP ofseti: {off} ms");
    }

    let cfg = hyperliquid_api::Config::from_env()?;
    // ENGINE_COINS verilmez/boş/"all" → TÜM AKTİF EVREN
    let mut ing_cfg = data::IngestorConfig {
        shard_count: 2,
        min_day_volume_usd: 0.0,
        l2book_top_n: 5,
        max_coins: None,
        ..Default::default()
    };
    let coins: Vec<String> = match std::env::var("ENGINE_COINS") {
        Ok(v) if !v.trim().is_empty() && !v.trim().eq_ignore_ascii_case("all") => {
            let c: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            ing_cfg.max_coins = Some(c.len());
            c
        }
        _ => {
            // Önce ingestor'dan universe al, sonra engine coins'a ata
            let ing = data::Ingestor::new(cfg.clone(), ing_cfg.clone()).await?;
            let names = ing.universe.names();
            tracing::info!("ENGINE_COINS boş → tam evren: {} coin (ingestor da aynı sete abone)", names.len());
            names
        }
    };
    let interval: u64 = std::env::var("ENGINE_INTERVAL")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let min_score: f64 = std::env::var("ENGINE_MIN_SCORE")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(0.30);
    let top_n: usize = std::env::var("ENGINE_TOP_N")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(5);

    // EĞİTİM/İŞLEM ZAMAN DİLİMİ AYNI OLMALI. Kelly eğitimi 1h mumlarla
    // yapıldı (4321 bar); 1m'de stratejilerin çoğu (Elliott dahil) 300
    // barlık pencerede sinyal üretemiyor → ağırlıkların tamamı ölü kalıyor.
    let hist_bars: usize = std::env::var("ENGINE_HIST_BARS")
        .ok().and_then(|v| v.parse().ok())
        .unwrap_or_else(|| if interval >= 3600 { 4400 } else { 300 });
    // Döngü periyodu. `ENGINE_INTERVAL` MUM interval'idır (1h stratejisi için
    // 3600) ve döngü periyodunu KONTROL ETMEZ — daha önce 5 sn sabitti.
    let cycle_secs: u64 = std::env::var("ENGINE_CYCLE_SECS")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    tracing::info!(
        "BOT başlıyor: coins={} bar_interval={interval}s döngü={cycle_secs}s hist_bars={hist_bars} min_score={min_score}",
        coins.len()
    );
    // ================= TRAILING STOP (§8.3.15) =================
    // Varsayılan AÇIK. `TRAIL_ON=0` ile kapatılır (sabit stop davranışı).
    // `TRAIL_ATR_K` stop mesafesinin ATR çarpanı; giriş stop'u ile aynı
    // tutulur (1.5) ki trailing ilk adımda zaten gevşetmesin.
    let trail_on = std::env::var("TRAIL_ON")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let trail_atr_k: f64 = std::env::var("TRAIL_ATR_K")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.5);
    let trail_min_step: f64 = std::env::var("TRAIL_MIN_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.25);
    tracing::info!(
        "TRAİLİNG STOP: on={trail_on} k={trail_atr_k}×ATR min_adım={trail_min_step}×ATR          (TP sabit 3×ATR — trailing TP kâr elde etmeyi düşürür, §8.3.15)"
    );
    let lc = LoopConfig {
        coins,
        interval_secs: interval,
        hist_bars,
        min_score,
        batch_window_ms: 100,
        max_position_usd: 50.0,
        top_n,
        trail_on,
        trail_atr_k,
        trail_min_step,
    };
    let mut engine = Engine::new(cfg.clone(), lc).await?;

    // akış + döngü
    let ingestor = data::Ingestor::new(cfg.clone(), ing_cfg).await?;
    let _rx = ingestor.start(cfg.clone());

    // KULLANICI EVENT AKIŞI (orderUpdates + userEvents) — gerçek fill geri beslemesi
    let user = std::env::var("USER_ADDRESS").unwrap_or_default();
    // WS kuyruk izleme için sarmalayıcı
    struct MonitoredRx {
        rx: tokio::sync::mpsc::Receiver<hyperliquid_api::websocket::WsEvent>,
        pending: Arc<AtomicUsize>,
    }
    impl MonitoredRx {
        fn new(rx: tokio::sync::mpsc::Receiver<hyperliquid_api::websocket::WsEvent>, pending: Arc<AtomicUsize>) -> Self {
            Self { rx, pending }
        }
        async fn recv(&mut self) -> Option<hyperliquid_api::websocket::WsEvent> {
            let event = self.rx.recv().await;
            if event.is_some() {
                self.pending.fetch_sub(1, Ordering::Relaxed);
            }
            event
        }
        fn pending(&self) -> usize {
            self.pending.load(Ordering::Relaxed)
        }
    }

    // S4: WS kuyruk izleme için sayaç
    let pending_ws = Arc::new(AtomicUsize::new(0));
    let mut oe_rx = {
        if user.is_empty() {
            None
        } else {
            let (tx, rx): (tokio::sync::mpsc::Sender<hyperliquid_api::websocket::WsEvent>, tokio::sync::mpsc::Receiver<hyperliquid_api::websocket::WsEvent>) = tokio::sync::mpsc::channel(2048);
            let subs = vec![
                serde_json::json!({ "type": "orderUpdates", "user": user.to_ascii_lowercase() }),
                serde_json::json!({ "type": "userEvents", "user": user.to_ascii_lowercase() }),
            ];
            let (tx, rx): (tokio::sync::mpsc::Sender<hyperliquid_api::websocket::WsEvent>, tokio::sync::mpsc::Receiver<hyperliquid_api::websocket::WsEvent>) = tokio::sync::mpsc::channel(2048);
            let (monitored_tx, monitored_rx) = tokio::sync::mpsc::channel(2048);
            
            // Spawn 1: WS client
            let cfg_ws = cfg.clone();
            let subs_clone = subs.clone();
            let ws_tx = tx.clone();
            tokio::spawn(async move {
                let ws = hyperliquid_api::WsClient::new(cfg_ws);
                ws.run(subs.clone(), ws_tx).await.ok();
            });
            
            // Spawn 2: Bridge WS events -> monitored channel
            let pending_bridge = Arc::new(AtomicUsize::new(0));
            let pending_bridge_clone = pending_bridge.clone();
            let monitored_tx = monitored_tx.clone();
            let mut rx = rx;
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    pending_bridge_clone.fetch_add(1, Ordering::Relaxed);
                    if monitored_tx.send(event).await.is_err() { break; }
                }
            });
            
            Some((monitored_rx, pending_bridge))
        }
    };

    let mut cycle = tokio::time::interval(std::time::Duration::from_secs(cycle_secs));
    // P1-F: kill-switch taksonomisi. Eşik `ENGINE_CRIT_THRESHOLD` (varsayılan 5)
    // ama artık yalnız **karar-kritik** hataları sayar.
    let crit_thr = std::env::var("ENGINE_CRIT_THRESHOLD")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let mut health = engine::track::Health::new(crit_thr);
    let mut ntp_cycle = tokio::time::interval(std::time::Duration::from_secs(1800)); // 30 dk'da resync
    ntp_cycle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Ağırlık dosyası: **yalnız `origin:"live"`** olan kabul edilir.
    // Varsayılan yol `ensemble_weights_live.json` (eski `ensemble_weights.json`
    // backtest çıktısıydı ve look-ahead biaslıydı — S19 kapandı).
    let weights_in = std::env::var("ENSEMBLE_WEIGHTS_IN")
        .unwrap_or_else(|_| "/root/hyperliquid/config/ensemble_weights_live.json".into());
    // CANLI tahteribaf: aynı dosya (in=out). `origin:"live"` şeması yazılır,
    // restart'ta aynı dosya okunur → ağırlıklar kalıcı.
    let weights_out = std::env::var("ENSEMBLE_LIVE_WEIGHTS_OUT")
        .unwrap_or_else(|_| "/root/hyperliquid/config/ensemble_weights_live.json".into());
    if let Some(par) = std::path::Path::new(&weights_out).parent() {
        let _ = std::fs::create_dir_all(par);
    }
    // Öğrenilen tick hassasiyeti (coin'e özel, meta'da yok) — restart'ta unutulmasın
    let tick_cache_out = std::env::var("ENGINE_TICK_CACHE")
        .unwrap_or_else(|_| "/root/hyperliquid/config/tick_cache.json".into());
    engine.executor.load_px_dec(&tick_cache_out);
    // keşif şeridi: 0'a düşen stratejiler küçük lotla canlıda sınanır
    let explore_on = std::env::var("ENGINE_EXPLORE")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("off"))
        .unwrap_or(true);
    // ORIGİN KAPISI: backtest çıktısı (`origin != "live"`) **reddedilir**.
    // Neden: o ağırlıklar look-ahead biased motordan geliyordu (S19). Motor
    // kaldırıldı; yine de bayat dosya sessizce yüklenirse sistem "eğitilmiş"
    // görünür ama veri biaslı olur — en kötü durum (görünmez yanlış).
    match std::fs::read_to_string(&weights_in) {
        Ok(txt) => match serde_json::from_str::<serde_json::Value>(&txt) {
            Ok(v) => {
                let origin = v
                    .get("origin")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(yok)");
                if origin == "live" {
                    let n = v
                        .get("weights")
                        .and_then(|w| w.as_object())
                        .map(|o| o.len())
                        .unwrap_or(0);
                    engine.load_live_weights(&v);
                    tracing::info!(
                        "[LEARN] canlı ağırlıklar yüklendi: {n} strateji ← {weights_in} (origin=live)"
                    );
                } else {
                    tracing::warn!(
                        "[LEARN] {weights_in} REDDEDİLDİ: origin={origin:?} ≠ \"live\" \
                         (backtest çıktısı kaldırıldı) — nötr cold-start (w=1.0)"
                    );
                }
            }
            Err(e) => tracing::warn!("[LEARN] {weights_in} parse edilemedi: {e} — nötr cold-start"),
        },
        Err(e) => tracing::warn!(
            "[LEARN] ağırlık dosyası yok ({e}) — nötr cold-start: {} strateji × w=1.0; persistence=ON",
            strategy::STRATEGY_COUNT
        ),
    }
    engine.explore_on = explore_on;
    engine.apply_effective_weights();

    engine.log_cold_start();
    let mut dump_cycle = tokio::time::interval(std::time::Duration::from_secs(300));
    dump_cycle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // [LEARN] tablosu 60 sn'de bir (döngü 5 sn → 12 döngüde bir). Log spam olmaz.
    let mut learn_table_cycle = tokio::time::interval(std::time::Duration::from_secs(60));
    learn_table_cycle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // S4: WS kuyruk izleme (her 60 sn)
    let mut ws_q_cycle = tokio::time::interval(std::time::Duration::from_secs(60));
    ws_q_cycle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // P1-2: Restart exponential backoff
    let mut restart_count_last_hour: Vec<u64> = Vec::new();
    const RESTART_BASE_SECS: u64 = 30;
    const RESTART_MAX_SECS: u64 = 300;

    'restart: loop {
        let mut rx = ingestor.start(cfg.clone());
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Some(ev) => {
                            let _ = engine.ingest_step(ev).await;
                        }
                        None => {
                            // Kanal kapandı = WS akışı kesildi. `websocket.rs`
                            // içindeki otomatik reconnect başarısız olup task
                            // düştüğünde bu olur; `ingestor.start` yeniden
                            // kurulur. P1-F: **bu KILL DEĞİL** — sayaç yalnız
                            // WS fırtınası (>10 / 5 dk) → ExitsOnly.
                            tracing::warn!("akış kapandı — yeniden kuruluyor");
                            if health.on_ws_reconnect() {
                                tracing::error!(
                                    "WS FIRTINASI: {} kopma / 5 dk → kill YOK, ExitsOnly \
                                     (giriş kapalı; çıkışlar + borsa stop açık)",
                                    health.ws_recent()
                                );
                            }
                            // P1-2: Exponential backoff before restart
                            let now = hyperliquid_api::now_ms();
                            restart_count_last_hour.retain(|t| now.saturating_sub(*t) < 3_600_000);
                            restart_count_last_hour.push(now);
                            if restart_count_last_hour.len() >= 5 {
                                tracing::error!(
                                    "CRITICAL: {} restart / saat ≥ 5 → operatör müdahalesi gerekli",
                                    restart_count_last_hour.len()
                                );
                                break 'restart;
                            }
                            let attempt = restart_count_last_hour.len();
                            let delay = RESTART_BASE_SECS * 2_u64.saturating_pow(attempt as u32 - 1);
                            let delay = delay.min(RESTART_MAX_SECS);
                            tracing::error!(
                                "RESTART #{}: {} sn bekleniyor (exponential backoff, max {} sn)",
                                attempt, delay, RESTART_MAX_SECS
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                            continue 'restart;
                        }
                    }
                }
                _ = ntp_cycle.tick() => {
                    if let Some(o) = time::sync(time::DEFAULT_SERVERS, std::time::Duration::from_secs(3)) {
                        tracing::info!("NTP resync: {o} ms");
                    }
                }
                _ = dump_cycle.tick() => {
                    let w = engine.tracker_weights_json();
                    if !w.is_empty() && w != "{}" {
                        let _ = std::fs::write(&weights_out, w);
                    }
                    let _ = engine.executor.save_px_dec(&tick_cache_out);
                    // P0-B: tracker durumu diske. YAZILMAZSA restart'ta
                    // tüm canlı öğrenme kaybolur ve bot tekrar cold-start'a
                    // düşer — yani hiç öğrenemez. Atomik yazım (save_state).
                    engine.save_tracker_state();
                }
                _ = learn_table_cycle.tick() => {
                    engine.log_learn_table();
                }
                // S4: WS kuyruk izleme (her 60 sn)
                _ = ws_q_cycle.tick() => {
                    if let Some((_, ref pending)) = oe_rx {
                        let depth = pending.load(Ordering::Relaxed);
                        let cap = 2048;
                        let pct = (depth * 100) / cap;
                        tracing::info!("[WS-Q] doluluk=%{} | bekleyen={} | toplam_kuyruk={}", pct, depth, cap);
                        if pct >= 80 {
                            tracing::warn!("[WS-Q] UYARI: kuyruk %{} dolu ({}/{})", pct, depth, cap);
                        }
                    }
                }
                oe_ev = async {
                    match oe_rx.as_mut() {
                        Some((ch, _)) => ch.recv().await,
                        None => std::future::pending::<Option<hyperliquid_api::websocket::WsEvent>>().await,
                    }
                } => {
                    if let Some(oe) = oe_ev {
                        engine.ingest_oe(&oe.channel, &oe.data);
                    }
                }
                _ = cycle.tick() => {
                    let out = match engine.run_cycle().await {
                        Ok(o) => {
                            health.on_cycle_ok();
                            engine.set_health_ok(health.entries_allowed());
                            o
                        }
                        Err(e) => {
                            // P1-F TAKSONOMİ: yalnız karar-kritik hatalar
                            // sayılır. WS reconnect / geçici REST timeout /
                            // 429 / stop-yerleştirme hatası **sayılmaz** —
                            // eski kural bunları sayıyor ve testnet'in normal
                            // WS düşüşleri sistemi öldürüyordu.
                            let kill = health.on_error(&e.to_string());
                            let crit = health.crit_streak();
                            engine.set_health_ok(health.entries_allowed());
                            if kill {
                                tracing::error!(
                                    "KILL-SWITCH: {crit} karar-kritik hata üst üste — \
                                     ACİL DÜZLEŞTİRME, sonra duraklatılıyor. Son hata: {e}"
                                );
                                // EN ÖLÜMCÜL KOMBİNASYON: kill-switch + borsada
                                // SL yok = sahipsiz pozisyon. Önce düzleştir,
                                // sonra duraklat.
                                engine.flatten_positions().await;
                                let left = engine.executor.positions()
                                    .values().filter(|p| p.szi.abs() > 1e-12).count();
                                if left > 0 {
                                    tracing::error!(
                                        "CRITICAL: düzleştirme sonrası {left} pozisyon AÇIK. \
                                         Bot duruyor — bu pozisyonlar ARTIK BORSA STOP'UNA \
                                         EMANET (bot içi kontrol yok). Elle müdahale gerek."
                                    );
                                }
                                break 'restart;
                            }
                            // P1-2: Exponential backoff for kill-switch restart too
                            let now = hyperliquid_api::now_ms();
                            restart_count_last_hour.retain(|t| now.saturating_sub(*t) < 3_600_000);
                            restart_count_last_hour.push(now);
                            if restart_count_last_hour.len() >= 5 {
                                tracing::error!(
                                    "CRITICAL: {} restart / saat ≥ 5 (kill-switch) → operatör müdahalesi gerekli",
                                    restart_count_last_hour.len()
                                );
                                break 'restart;
                            }
                            let attempt = restart_count_last_hour.len();
                            let delay = RESTART_BASE_SECS * 2_u64.saturating_pow(attempt as u32 - 1);
                            let delay = delay.min(RESTART_MAX_SECS);
                            tracing::error!(
                                "RESTART (kill-switch) #{}: {} sn bekleniyor (exponential backoff, max {} sn)",
                                attempt, delay, RESTART_MAX_SECS
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                            match engine::track::classify(&e.to_string()) {
                                engine::track::ErrClass::Transient => {
                                    tracing::warn!("geçici hata (sayılmadı, {crit}/{thr}): {e}", thr = 5);
                                }
                                engine::track::ErrClass::Critical => {
                                    tracing::error!("karar-kritik hata ({crit}/5): {e}");
                                }
                            }
                            continue;
                        }
                    };
                    if !out.is_empty() {
                        eprintln!("{}", engine::engine_loop::fmt_signals(&out));
                    }
                }
            }
        }
    }
    tracing::info!("bot kapanıyor");
    Ok(())
}
