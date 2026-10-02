//! BOT — ana binary: NTP → ingestor WS → engine döngüsü.
//! ENV:
//!   ENGINE_COINS=BTC,ETH,SOL   (döngüde koşacak coinler)
//!   ENGINE_INTERVAL=60         (sinyal mum aralığı, sn)
//!   ENGINE_MIN_SCORE=0.3       (ensemble eşiği)
//! Çalıştırma süresi yok — Ctrl+C ile kes.

use engine::engine_loop::{Engine, LoopConfig};
use utils::{logger, time};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logger::init();
    if let Some(off) = time::sync(time::DEFAULT_SERVERS, std::time::Duration::from_secs(3)) {
        // Döngü periyodu. `ENGINE_INTERVAL` MUM interval'idır (1h stratejisi için
    // 3600) ve döngü periyodunu KONTROL ETMEZ — daha önce 5 sn sabitti.
    let cycle_secs: u64 = std::env::var("ENGINE_CYCLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    tracing::info!("NTP ofseti: {off} ms");
    }

    let cfg = hyperliquid_api::Config::from_env()?;
    // ENGINE_COINS verilmez/boş/"all" → TÜM AKTİF EVREN (bırakılırsa eskiden sadece BTC)
    let coins: Vec<String> = match std::env::var("ENGINE_COINS") {
        Ok(v) if !v.trim().is_empty() && !v.trim().eq_ignore_ascii_case("all") => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => {
            // Emir defteri aboneliği KAPALIYSA bid/ask = 0 gelir; tahsis
            // fiyatı mum kapanışına düşer. Gerçekçi gözlem için L2 açık.
            let icfg = data::IngestorConfig { l2book_top_n: 20, ..Default::default() };
            let ing = data::Ingestor::new(cfg.clone(), icfg).await?;
            let names = ing.universe.names();
            tracing::info!("ENGINE_COINS boş → tam evren: {} coin", names.len());
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
    let ingestor_cfg = data::IngestorConfig {
        shard_count: 2,
        min_day_volume_usd: 0.0,
        l2book_top_n: 5,
        max_coins: Some(30),
        ..Default::default()
    };
    let ingestor = data::Ingestor::new(cfg.clone(), ingestor_cfg).await?;
    let _rx = ingestor.start(cfg.clone());

    // KULLANICI EVENT AKIŞI (orderUpdates + userEvents) — gerçek fill geri beslemesi
    let user = std::env::var("USER_ADDRESS").unwrap_or_default();
    let mut oe_rx = {
        if user.is_empty() {
            None
        } else {
            let ws = hyperliquid_api::WsClient::new(cfg.clone());
            let (tx, rx) = tokio::sync::mpsc::channel(2048);
            let subs = vec![
                serde_json::json!({ "type": "orderUpdates", "user": user.to_ascii_lowercase() }),
                serde_json::json!({ "type": "userEvents", "user": user.to_ascii_lowercase() }),
            ];
            tokio::spawn(async move { ws.run(subs, tx).await.ok(); });
            Some(rx)
        }
    };

    let mut cycle = tokio::time::interval(std::time::Duration::from_secs(cycle_secs));
    let mut err_streak = 0usize;
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

    'restart: loop {
        let mut rx = ingestor.start(cfg.clone());
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Some(ev) => { let _ = engine.ingest_step(ev).await; }
                        None => {
                            tracing::warn!("akış kapandı — 3 sn sonra yeniden kur");
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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
                oe_ev = async {
                    match oe_rx.as_mut() {
                        Some(ch) => ch.recv().await,
                        None => std::future::pending::<Option<hyperliquid_api::websocket::WsEvent>>().await,
                    }
                } => {
                    if let Some(oe) = oe_ev {
                        engine.ingest_oe(&oe.channel, &oe.data);
                    }
                }
                _ = cycle.tick() => {
                    let out = match engine.run_cycle().await {
                        Ok(o) => { err_streak = 0; o }
                        Err(e) => {
                            err_streak += 1;
                            tracing::error!("döngü hatası ({err_streak}): {e}");
                            if err_streak >= 5 {
                                // EN ÖLÜMCÜL KOMBİNASYON: kill-switch + borsada
                                // SL yok = sahipsiz pozisyon. Önce düzleştir,
                                // sonra duraklat.
                                tracing::error!("5 hata üst üste — ACİL DÜZLEŞTİRME, sonra duraklatılıyor");
                                engine.flatten_positions().await;
                                break 'restart;
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
