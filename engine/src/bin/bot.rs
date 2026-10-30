//! BOT — üretim binary'si: NTP → ingestor WS → engine döngüsü.
//!
//! Sorumluluklar:
//! 1. Tüm konfig ENV'den (ENGINE_* / TRAIL_* / LEARN_*); secret .env'den
//!
//! (dotenvy) — restart'ta export gereksinimi yok.
//! 2. Canlı ağırlık dosyası doğrulaması: `origin:"live"` zorunlu; bozuk
//!
//! dosya → AÇILIŞ BAŞARISIZ (sessiz cold-start = öğrenme kaybı);
//! dosya yok → nötr cold-start.
//! 3. WS fırtınası (>10 kopma/5dk) → ExitsOnly (kill YOK); health → engine.
//! 4. Periyodik dump (300 sn): etkin ağırlıklar (ATOMİK tmp+rename),
//!
//! tick cache, tracker state.
//! 5. Restart exponential backoff (30..300 sn).
//! 6. Öğrenme dönemi bootstrap: testnet endpoint teyidi, FRESH_START
//!    wipe (tracker/weights/epoch/risk + flatten + arşiv), epoch
//!    yükleme, risk hafızası yükleme — normal restart state KORUR.

use engine::engine_loop::{Engine, LoopConfig};
use utils::{logger, time};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    logger::init();
    if let Some(off) = time::sync(time::DEFAULT_SERVERS, std::time::Duration::from_secs(3)) {
        tracing::info!("NTP ofseti: {off} ms");
    }

    let cfg = hyperliquid_api::Config::from_env()?;

    // ================= — TESTNET ÖĞRENME DÖNEMİ (bootstrap) =================
    // "Sıfırdan başlamak" = testin başlangıcında temizlemek; her restart'ta
    // state silmek DEĞİL. Sayaç/epoch/öğrenme kalıcıdır.
    let fresh_env = std::env::var("TESTNET_FRESH_START")
        .ok()
        .is_some_and(|v| v == "1");
    let epoch_path = std::env::var("LEARNING_EPOCH_PATH")
        .unwrap_or_else(|_| "/root/hyperliquid/config/learning_epoch.json".into());
    let tracker_path = std::env::var("TRACK_STATE_PATH")
        .unwrap_or_else(|_| "/root/hyperliquid/config/track_state.json".into());
    let trade_exits_path = std::env::var("TRADE_EXITS_PATH")
        .unwrap_or_else(|_| "/root/hyperliquid/config/trade_exits.jsonl".into());
    let weights_path = std::env::var("ENSEMBLE_WEIGHTS_IN")
        .unwrap_or_else(|_| "/root/hyperliquid/config/ensemble_weights_live.json".into());
    let is_testnet = cfg.network == "testnet";
    // ORTAM TESPİTİ: mainnet = NETWORK=mainnet İLE BİRLİKTE MAINNET_CONFIRMED=1
    // (çift anahtar: tek başına NETWORK yeterli DEĞİL — yanlışlıkla canlıya
    // açılma koruması). Onaysız/eksik her koşu testnet kuralına düşer.
    let environment = if cfg.network == "mainnet" {
        if std::env::var("MAINNET_CONFIRMED").as_deref() != Ok("1") {
            anyhow::bail!(
                "[MAINNET] NETWORK=mainnet ama MAINNET_CONFIRMED=1 yok — \
                 canlıya geçiş çift-onay ister (bilinçli operatör eylemi)"
            );
        }
        "mainnet"
    } else {
        "testnet"
    };
    // YAPISAL KORUMA: environment ↔ endpoint eşleşmesi (testnet epoch canlı
    // endpoint'e karşı başlamaz; mainnet ise gerçek mainnet hostu ister).
    engine::epoch::assert_endpoint_for(environment, &cfg.network, &cfg.api_url)?;

    // Risk hafızası yolu: WIPE ve normal-yükleme AYNI kaynağı kullanır —
    // RISK_STATE_PATH env'i; custom path'te de fresh reset risk hafızasını
    // tamamen temizler ve restart aynı hafızayı yükler.
    let risk_state_path = std::env::var("RISK_STATE_PATH")
        .unwrap_or_else(|_| "/root/hyperliquid/config/risk_state.json".into());
    let decision = engine::epoch::fresh_start_decision(
        fresh_env,
        std::path::Path::new(&epoch_path).exists(),
        is_testnet,
    );
    match decision {
        engine::epoch::FreshDecision::Invalid => {
            anyhow::bail!("[TESTNET] TESTNET_FRESH_START yalnız testnet endpoint üzerinde çalışabilir — canlı anahtarlarla flatten/reset İSTENEMEZ");
        }
        engine::epoch::FreshDecision::Wipe => {
            tracing::warn!("[TESTNET] FRESH_START: temiz deney başlatılıyor");
            // 1) eski tracker state (öğrenme) — silinir → nötr w=1.0
            match std::fs::remove_file(&tracker_path) {
                Ok(()) => tracing::info!("[TESTNET] eski tracker state temizlendi: {tracker_path}"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => anyhow::bail!("[TESTNET] tracker state silinemedi: {e}"),
            }
            // 1b) risk hafızası — silinir (yeni deney = yeni drawdown hafızası);
            // AYNI çözülen yol (ENV RISK_STATE_PATH) — hard-coded DEĞİL
            match std::fs::remove_file(&risk_state_path) {
                Ok(()) => tracing::info!("[TESTNET] eski risk hafızası temizlendi"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => anyhow::bail!("[TESTNET] risk hafızası silinemedi: {e}"),
            }
            // 2) eski canlı ağırlık snapshot — silinir (look-ahead'den temiz)
            match std::fs::remove_file(&weights_path) {
                Ok(()) => tracing::info!(
                    "[TESTNET] eski canlı ağırlık snapshot temizlendi: {weights_path}"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => anyhow::bail!("[TESTNET] ağırlık snapshot silinemedi: {e}"),
            }
            // 3) eski trade_exits — ARŞİVLE (kayıp yok, yeni döneme karışmaz)
            if std::path::Path::new(&trade_exits_path).exists() {
                let ts = hyperliquid_api::now_ms();
                let archive = format!("{trade_exits_path}.{ts}.prev");
                std::fs::rename(&trade_exits_path, &archive)
                    .map_err(|e| anyhow::anyhow!("[TESTNET] trade_exits arşivlenemedi: {e}"))?;
                tracing::info!("[TESTNET] eski trade_exits arşivlendi: {archive}");
            }
            // 4) yeni epoch dosyası (Engine::new yükler; burada yazılırsa
            // "ilk çalıştırma" mührü atılmış olur → sonraki restart KORUR)
            let epoch = engine::epoch::LearningEpoch::new_for_environment(
                environment,
                hyperliquid_api::now_ms().to_string(),
                hyperliquid_api::now_ms(),
            );
            epoch
                .save(&epoch_path)
                .map_err(|e| anyhow::anyhow!("[TESTNET] yeni epoch yazılamadı: {e}"))?;
            tracing::info!(
                "[TESTNET] yeni öğrenme dönemi: {} (hedef {} GERÇEK kapanmış işlem)",
                epoch.epoch_id,
                engine::epoch::LEARNING_TRADES_TARGET
            );
        }
        engine::epoch::FreshDecision::KeepExisting => {
            if fresh_env {
                tracing::info!(
                    "[TESTNET] FRESH_START istendi ama öğrenme dönemi zaten aktif → state KORUNDU                      (reset yalnız ilk çalıştırmada olur)"
                );
            }
        }
    }

    // TEK KAYNAKLI EVREN. Sıralama ÖNEMLİ:
    // 1. `ENGINE_COINS` spec'i çözülür (tam sayı → TopByVolume, liste → Exact)
    // 2. aktif evren çekilir (delist + hacim filtresi) — `max_coins` ARTIK YOK
    // 3. `TradingUniverse::build` evreni BİR KEZ çözer (sessiz kırpma yok)
    // 4. engine listesi, ingestor aboneliği, risk ve WS hepsi AYNI nesneden
    //
    // ÖNCE: `max_coins = Some(n)` ingestor kendi TOP-N'ini seçiyordu, engine
    // config listesini kullanıyordu. Sayılar eşit, SET'ler farklıydı.
    let ing_cfg = data::IngestorConfig {
        shard_count: 2,
        min_day_volume_usd: 0.0,
        l2book_top_n: std::env::var("ENGINE_L2BOOK_TOP_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10000), // 0 = kapalı; varsayılan = tam evren (L2 imbalance kanıtı)
        ..Default::default()
    };
    let spec = data::UniverseSpec::parse(std::env::var("ENGINE_COINS").ok().as_deref())?;
    let active =
        data::ingestor::load_universe(&hyperliquid_api::Info::new(cfg.clone())?, &ing_cfg).await?;
    let universe = data::TradingUniverse::build(&active, spec)?;
    let coins: Vec<String> = universe.names();
    // Katman doğrulaması: engine listesi ile ingestor aboneliği birebir aynı
    // olmalı; değilse CONFIG ERROR logu + bot başlamaz (sessiz kırpma yasak).
    universe.verify_layers(&ing_cfg)?;
    tracing::info!(
        "[UNIVERSE] {} coin evren çözüldü (spec={}) — engine/ingestor/risk/WS aynı kaynaktan",
        coins.len(),
        universe.spec().describe()
    );
    // FALLBACK = BELGELENMİŞ varsayılan (1h mum): .env kaybolursa bot
    // sessizce farklı bir bot (1m/5s) OLMAZ.
    let interval: u64 = std::env::var("ENGINE_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let min_score: f64 = std::env::var("ENGINE_MIN_SCORE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.30);
    let top_n: usize = std::env::var("ENGINE_TOP_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    // EĞİTİM/İŞLEM ZAMAN DİLİMİ AYNI OLMALI. Kelly eğitimi 1h mumlarla
    // yapıldı (4321 bar); 1m'de stratejilerin çoğu (Elliott dahil) 300
    // barlık pencerede sinyal üretemiyor → ağırlıkların tamamı ölü kalıyor.
    let hist_bars: usize = std::env::var("ENGINE_HIST_BARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if interval >= 3600 { 4400 } else { 300 });
    // Döngü periyodu. `ENGINE_INTERVAL` MUM interval'idır (1h stratejisi için
    // 3600) ve döngü periyodunu KONTROL ETMEZ — daha önce 5 sn sabitti.
    let cycle_secs: u64 = std::env::var("ENGINE_CYCLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    tracing::info!(
        "BOT başlıyor: coins={} bar_interval={interval}s döngü={cycle_secs}s hist_bars={hist_bars} min_score={min_score}",
        coins.len()
    );
    // ================= TRAILING STOP =================
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
    // TP mesafesi (tp_atr_mult/sl_atr_mult) × GERÇEK risk mesafesidir
    // (min 2×risk kapılı); giriş dolumunda reduce-only TP trigger borsaya
    // asılır, fill → ExitReason::TakeProfit. Trailing yalnız SL'yi
    // ratchet'ler.
    tracing::info!(
        "TRAİLİNG STOP: on={trail_on} k={trail_atr_k}×ATR min_adım={trail_min_step}×ATR          \
         (yalnız SL ratchet'lenir; TP GERÇEK kapanıştır: giriş dolumunda reduce-only TP trigger asılır — entry_eff ± max(tp/sl×risk, 2×risk), §4)"
    );
    let max_position_usd: f64 = std::env::var("ENGINE_MAX_POSITION_USD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(50.0);
    let lc = LoopConfig {
        coins,
        cycle_secs,
        max_position_usd,
        interval_secs: interval,
        hist_bars,
        min_score,
        batch_window_ms: 100,
        top_n,
        trail_on,
        trail_atr_k,
        trail_min_step,
    };
    let mut engine = Engine::new(cfg.clone(), lc).await?;

    // Risk hafızası KALICI: peak_equity + halted restart'ta yüklenir.
    // (Yüklenmezse 100→80'de halt eden hesap, restart'ta 80'i yeni peak
    // kabul ederek drawdown korumasını sıfırlamış olur.)
    match engine.risk.load_state(&risk_state_path) {
        engine::risk_manager::RiskStateLoad::NotFound => {
            tracing::info!("[RISK] risk hafızası yok — fresh başlangıç (ilk koşu normal)");
        }
        engine::risk_manager::RiskStateLoad::Loaded => {
            tracing::info!("[RISK] hafıza yüklendi: {risk_state_path}");
        }
        engine::risk_manager::RiskStateLoad::Corrupt => {
            anyhow::bail!(
                "[RISK] risk hafızası BOZUK/OKUNAMAZ ({risk_state_path}) — \
                 drawdown koruması kaybı riski; mainnet başlangıcı reddedildi. \
                 Dosyayı onarın veya TESTNET_FRESH_START ile temizleyin."
            );
        }
    }

    // WIPE modunda temiz taban — TÜM pozisyonlar kapatılır ve açık
    // emirler iptal edilir (ENGINE_FLATTEN'den bağımsız; bu bir deney
    // başlangıcıdır). Normal restart'ta BUNLAR ÇALIŞMAZ.
    if decision == engine::epoch::FreshDecision::Wipe {
        engine.executor.cancel_all_open_orders().await?;
        engine.flatten_positions().await;
        tracing::warn!(
            "[TESTNET] hesap düzleşti + açık emirler temizlendi — temiz deney başlangıcı"
        );
    }

    // akış + döngü
    let ingestor = data::Ingestor::with_universe(cfg.clone(), ing_cfg, universe.clone()).await?;
    let _rx = ingestor.start(cfg.clone());

    // KULLANICI EVENT AKIŞI (orderUpdates + userEvents) — gerçek fill geri beslemesi
    let user = std::env::var("USER_ADDRESS").unwrap_or_default();
    let mut oe_rx = {
        if user.is_empty() {
            None
        } else {
            let (tx, rx) = tokio::sync::mpsc::channel(2048);
            let subs = vec![
                serde_json::json!({ "type": "orderUpdates", "user": user.to_ascii_lowercase() }),
                serde_json::json!({ "type": "userEvents", "user": user.to_ascii_lowercase() }),
            ];
            // SUPERVISOR (kanıtlanan kod: api::supervisor::keepalive + 4
            // runtime testi): kullanıcı akışı (fill geri beslemesi!) öldüyse
            // SESSİZ ölü değildir — loglanır + 5 sn sonra yeniden başlatılır.
            // (eskiden .ok() ile yutuluyordu: task ölürse bot fillsiz
            // çalışmaya devam ederdi ve kimse göremezdi.)
            let cfg_task = cfg.clone();
            tokio::spawn(hyperliquid_api::supervisor::keepalive(
                move |episode: u64| {
                    let _ = episode;
                    // Her bölüm kendi bağlantı-client'ıyla — içeride
                    // bekleyen gelecek dışarıdaki durumu ödünç almaz.
                    let cfg = cfg_task.clone();
                    let subs = subs.clone();
                    let tx = tx.clone();
                    async move { hyperliquid_api::WsClient::new(cfg).run(subs, tx).await }
                },
                std::time::Duration::from_secs(5),
                move |episode, r| {
                    match r {
                    Ok(()) => tracing::warn!("[WS] kullanıcı akışı döndü — bölüm {episode} yeniden başlatılıyor"),
                    Err(e) => tracing::error!("[WS] kullanıcı akışı ÖLDÜ: {e} — bölüm {episode} 5 sn sonra yeniden başlatılıyor (fill geri beslemesi bu arada REST recon'dan)"),
                }
                },
            ));
            Some(rx)
        }
    };

    let mut cycle = tokio::time::interval(std::time::Duration::from_secs(cycle_secs));
    // kill-switch taksonomisi: eşik `ENGINE_CRIT_THRESHOLD` (varsayılan 5)
    // ve yalnız **karar-kritik** hataları sayar.
    let crit_thr = std::env::var("ENGINE_CRIT_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let mut health = engine::track::Health::new(crit_thr);
    let mut ntp_cycle = tokio::time::interval(std::time::Duration::from_secs(1800)); // 30 dk'da resync
    ntp_cycle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Ağırlık dosyası: **yalnız `origin:"live"`** olan kabul edilir
    // (backtest-üretimli ağırlık dosyası look-ahead biaslıdır; kontrat
    // canlı kanıt sürecinden gelen ağırlıktır).
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
    // Neden: o ağırlıklar look-ahead biased motordan geliyordu . Motor
    // kaldırıldı; yine de bayat dosya sessizce yüklenirse sistem "eğitilmiş"
    // görünür ama veri biaslı olur — en kötü durum (görünmez yanlış).
    // Dosya VARSA doğrulanır; geçersizse **AÇILIŞ BAŞARISIZ** (sessiz
    // cold-start = öğrenilmiş ağırlıkların sessizce kaybı; 'deki
    // "universe uyuşmazlığı açılışı durdurur" disipliniyle aynı).
    match std::fs::read_to_string(&weights_in) {
        Ok(txt) => {
            let v = serde_json::from_str::<serde_json::Value>(&txt).map_err(|e| {
                anyhow::anyhow!("[LEARN] canlı ağırlık dosyası PARSE EDİLEMEDİ ({weights_in}): {e}")
            })?;
            let rep = engine.load_live_weights(&v).map_err(|e| {
                anyhow::anyhow!("[LEARN] canlı ağırlık dosyası GEÇERSİZ ({weights_in}): {e}")
            })?;
            if rep.defaulted.is_empty() {
                tracing::info!(
                    "[LEARN] canlı ağırlık dosyası doğrulandı: {weights_in} → {} strateji (origin=live)",
                    rep.applied
                );
            } else {
                tracing::info!(
                    "[LEARN] canlı ağırlık dosyası doğrulandı: {weights_in} → {} strateji; \
                     dosyada OLMAYAN {} strateji AÇIKÇA nötr 1.0: {:?}",
                    rep.applied,
                    rep.defaulted.len(),
                    rep.defaulted
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => tracing::info!(
            "[LEARN] ağırlık dosyası yok ({weights_in}) — nötr cold-start: {} strateji × w=1.0; persistence=ON",
            strategy::STRATEGY_COUNT
        ),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "[LEARN] canlı ağırlık dosyası okunamadı ({weights_in}): {e}"
            ))
        }
    }
    engine.explore_on = explore_on;
    engine.apply_effective_weights();

    engine.log_cold_start();
    let mut dump_cycle = tokio::time::interval(std::time::Duration::from_secs(300));
    dump_cycle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // [LEARN] tablosu 60 sn'de bir (döngü 5 sn → 12 döngüde bir). Log spam olmaz.
    let mut learn_table_cycle = tokio::time::interval(std::time::Duration::from_secs(60));
    learn_table_cycle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Restart exponential backoff
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
            // kurulur. **bu KILL DEĞİL** — sayaç yalnız
            // WS fırtınası (>10 / 5 dk) → ExitsOnly.
                                       tracing::warn!("akış kapandı — yeniden kuruluyor");
                                       if health.on_ws_reconnect() {
                                           tracing::error!(
                                               "WS FIRTINASI: {} kopma / 5 dk → kill YOK, ExitsOnly \
                                                (giriş kapalı; çıkışlar + borsa stop açık)",
                                               health.ws_recent()
                                           );
                                       }
            // Exponential backoff before restart
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
            // ATOMİK YAZIM: bu dosya restart'ta AÇILIŞI ETKİLİYOR
            // (geçersizse bot açılmaz). Yarım yazılmış dosya
            // kalıcı bozulma olurdu → tmp + rename.
                                   let tmp = format!("{weights_out}.tmp");
                                   if let Err(e) = std::fs::write(&tmp, w.as_bytes())
                                       .and_then(|()| std::fs::rename(&tmp, &weights_out))
                                   {
                                       tracing::error!(
                                           "[LEARN] canlı ağırlık dosyası yazılamadı ({weights_out}): {e} \
                                            — restart'ta bu ağırlıklar KAYBOLUR"
                                       );
                                   }
                               }
                               let _ = engine.executor.save_px_dec(&tick_cache_out);
            // tracker durumu diske. YAZILMAZSA restart'ta
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
                                   Ok(o) => {
                                       health.on_cycle_ok();
                                       engine.set_health_ok(health.entries_allowed());
                                       o
                                   }
                                   Err(e) => {
            // TAKSONOMİ: yalnız karar-kritik hatalar
            // sayılır. WS reconnect / geçici REST timeout /
            // 429 / stop-yerleştirme hatası **sayılmaz** —
            // testnet'in normal WS düşüşleri kill-switch
            // tetiklememelidir.
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
            // Exponential backoff for kill-switch restart too
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

#[cfg(test)]
mod supervisor_tests {
    // REGRESYON KORUMASI: kullanıcı-event akışı (fill geri beslemesi)
    // bir zamanlar `.ok()` ile sessizce ölebiliyordu — task ölürse bot
    // fillsiz çalışmaya devam ederdi ve kill-switch bunu saymazdı.
    // Aşağıdaki source-scan testleri bu bozulmayı tekrar edemez yapar.

    #[test]
    fn kullanici_akis_spawn_ok_yutma_yasak() {
        let src = include_str!("bot.rs");
        let desen = ".await.".to_owned() + "ok();";
        // ok() ile yutulan WS task deseni asla geri gelmemeli
        assert!(
            !src.contains(&desen),
            "WS task olumu ok() ile YUTULUYOR — sessiz veri kaybi riski"
        );
    }

    #[test]
    fn kullanici_akis_supervisor_dongusu_mevcut() {
        let src = include_str!("bot.rs");
        assert!(
            src.contains("kullanıcı akışı ÖLDÜ"),
            "kullanıcı-event WS supervisor logu yok — task ölümü görünmez olur"
        );
        // Döngünün kendisi artık KANITLANMIŞ helper'daydı:
        // api::supervisor::keepalive (+4 runtime testi: ölüm algılama,
        // respawn, event tüketimi, tek-atımlık değişmezliği).
        assert!(
            src.contains("supervisor::keepalive"),
            "kullanıcı akışı keepalive helper'ını kullanmıyor — respawn garantisi zayıflar"
        );
    }
}
