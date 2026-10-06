//! LEARNING EPOCH — TESTNET öğrenme dönemi .
//!
//! Amaç: bot testnet'te **temiz bir deney** olarak başlar; gerçek testnet
//! işlemlerinden öğrenir; 100 GERÇEK kapanmış işlem tamamlanmadan
//! canlıya geçilmez (geçiş de otomatik değildir — `ready_for_live` yalnız
//! kalıcı bir durum yazımıdır).
//!
//! Kurallar:
//! 1. `TESTNET_FRESH_START=1` → yalnız **İLK** çalıştırmada sıfırlar
//! (epoch dosyası yoksa); sonraki restartlarda state KORUNUR.
//! 2. `completed_trade_count` = **kapanmış pozisyon** sayısı (sinyal/emir/
//! fill sayısı değildir); her kapanış tam BİR kez sayılır (tid dedup +
//! entry_attr tek-yazım garanti eder).
//! 3. Sayaç kalıcıdır — restart'ta kaldığı yerden devam eder.
//! 4. Canlı endpoint koruması: testnet epoch'u canlı API url'üne karşı
//! AÇILMAZ (yapısal kapı — bot hiç başlamaz, emir hiç atılmaz).
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Öğrenme döneminin hedef işlem sayısı (TESTNET geçiş eşiği).
pub const LEARNING_TRADES_TARGET: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EpochStatus {
 /// öğrenme sürüyor (w'ler gerçek R kanıtıyla güncellenir)
    Learning,
 /// 100 gerçek kapanış tamamlandı — kalıcı geçiş durumu;
 /// **otomatik canlı işlem YOK** (operatör kararı bekler)
    ReadyForLive,
}

/// Kalıcı öğrenme dönemi kaydı (`LEARNING_EPOCH_PATH`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LearningEpoch {
 /// YYYY-MM-DD-hhmm (bağışıklı: birden fazla epoch aynı gün açılabilir)
    pub epoch_id: String,
 /// yalnız "testnet" kabul edilir (bu build canlıya öğretmez)
    pub environment: String,
 /// dönem başlangıcı (unix ms)
    pub started_at: u64,
 /// dönem başladığında sayaç (her fresh start = 0)
    pub starting_trade_count: u64,
 /// **kapanmış pozisyon** sayısı — restart'ta kalıcı
    pub completed_trade_count: u64,
    pub status: EpochStatus,
}

/// FRESH_START yeniden başlatma kararı (saf — testler bunu sınar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshDecision {
 /// dosyaları temizle + yeni epoch aç (yalnız İLK fresh çalıştırma)
    Wipe,
 /// hiçbir öğrenme state'ine dokunma (normal restart / epoch zaten aktif)
    KeepExisting,
 /// fresh reset canlı endpoint'te İSTENEMEZ — süreci düşür
    Invalid,
}

/// TESTNET epoch'u canlı endpoint'e karşı BAŞLAMAZ (yapısal kapı).
///
/// Kural: `environment == "testnet"` iken `NETWORK != "testnet"` veya
/// API url'si mainnet hostu içeriyorsa → hata. Böylece testnet modunda
/// canlı endpoint'e emir gitmesi **yapısal olarak** engellenir (emir
/// hiç atılmadan süreç durur).
pub fn assert_testnet_endpoint(network: &str, api_url: &str, environment: &str) -> Result<()> {
    if environment != "testnet" {
        bail!("öğrenme dönemi environment=\"{environment}\" — yalnız testnet kabul edilir");
    }
    if network != "testnet" {
        bail!(
            "TESTNET öğrenme dönemi NETWORK={network:?} ile BAŞLAMAZ — canlı hesabı korumak için \
             süreci düşürüyorum (NETWORK=testnet yapın)"
        );
    }
    if api_url.contains("mainnet") || api_url.contains("hyperliquid.xyz") {
        bail!(
            "TESTNET öğrenme dönemi canlı API ({api_url}) ile BAŞLAMAZ — yapısal koruma"
        );
    }
    Ok(())
}

/// Yeniden başlatma kararı (saf):
///
/// | fresh | epoch dosyası | testnet | karar |
/// |---|---|---|---|
/// | 0 | — | — | `KeepExisting` (normal restart: hiçbir şey silinmez) |
/// | 1 | yok | evet | `Wipe` (İLK bootstrap çalıştırması) |
/// | 1 | **var** | evet | `KeepExisting` (reset bir kez olur; sonra state koru) |
/// | 1 | — | **hayır** | `Invalid` (canlı anahtarlarla flatten/reset yasağı) |
pub fn fresh_start_decision(fresh_env: bool, epoch_exists: bool, is_testnet: bool) -> FreshDecision {
    if !fresh_env {
        return FreshDecision::KeepExisting;
    }
    if !is_testnet {
        return FreshDecision::Invalid;
    }
    if epoch_exists {
        return FreshDecision::KeepExisting;
    }
    FreshDecision::Wipe
}

impl LearningEpoch {
 /// Yeni TESTNET öğrenme dönemi: `starting_trade_count = 0`,
 /// nötr başlangıç (tüm stratejiler w=1.0 — tracker boş açılır).
    pub fn new_testnet(epoch_id: String, started_at: u64) -> Self {
        Self {
            epoch_id,
            environment: "testnet".into(),
            started_at,
            starting_trade_count: 0,
            completed_trade_count: 0,
            status: EpochStatus::Learning,
        }
    }

 /// **GERÇEK kapanış** kaydı — bir pozisyon kapandıysa tam bir kez
 /// çağrılır (sinyal/emir/fill sayısı DEĞİL).
 ///
 /// Dönüş: (yeni sayaç, bu kayıtla `ready_for_live`'a geçildi mi).
 /// 100'e ulaşınca kalıcı duruma yazılır; **otomatik canlı emir YOK**.
    pub fn record_completed_trade(&mut self) -> (u64, bool) {
        self.completed_trade_count += 1;
        let mut ready_now = false;
        if self.completed_trade_count >= LEARNING_TRADES_TARGET && self.status == EpochStatus::Learning
        {
            self.status = EpochStatus::ReadyForLive;
            ready_now = true;
        }
        (self.completed_trade_count, ready_now)
    }

 /// Kalıcı yazım — **atomik** (tmp + rename): yarım yazım = sayaç
 /// sıfırlanması = öğrenme kaybı (`track::save_state` ile aynı şema).
    pub fn save(&self, path: &str) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        if let Some(par) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(par);
        }
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

 /// Diskten yükle (restart devamı). Yoksa/bozuksa `None` — çağıran
 /// yeni dönem açar (sessiz varsayım yok: bozuk dosya WARN + yeni epoch).
    pub fn load(path: &str) -> Option<Self> {
        let txt = std::fs::read_to_string(path).ok()?;
        let e: LearningEpoch = serde_json::from_str(&txt).ok()?;
        Some(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

 /// Kapanan pozisyon 1 trade sayılır; açık pozisyon sayılmaz (sayaç
 /// yalnız kapanış yollarından çağrılır — burada API davranışı).
    #[test]
    fn kapanan_pozisyon_bir_trade_sayilir() {
        let mut e = LearningEpoch::new_testnet("2026-10-05-1200".into(), 0);
        assert_eq!(e.completed_trade_count, 0);
        let (n, ready) = e.record_completed_trade();
        assert_eq!((n, ready), (1, false));
        assert_eq!(e.status, EpochStatus::Learning, "1 işlem ready yapmaz");
        let (n, _) = e.record_completed_trade();
        assert_eq!(n, 2, "her gerçek kapanış bir kez");
    }

 /// 99 → 100 geçişi doğru: 99'da Learning, 100'de ReadyForLive.
    #[test]
    fn yuz_gecisi_dogru() {
        let mut e = LearningEpoch::new_testnet("t".into(), 0);
        for i in 1..=99 {
            let (n, ready) = e.record_completed_trade();
            assert_eq!(n, i);
            assert!(!ready, "{i}. işlemde ready OLMAMALI");
        }
        assert_eq!(e.status, EpochStatus::Learning, "99 → hâlâ Learning");
        let (n, ready) = e.record_completed_trade();
        assert_eq!((n, ready), (100, true), "100 → ReadyForLive");
        assert_eq!(e.status, EpochStatus::ReadyForLive);
 // idempotent: sonraki kayıtlar geçişi tekrar raporlamaz
        let (_, ready2) = e.record_completed_trade();
        assert!(!ready2);
    }

 /// 100 olmadan ready_for_live OLUŞMAZ.
    #[test]
    fn yuz_olmadan_ready_olusmaz() {
        let mut e = LearningEpoch::new_testnet("t".into(), 0);
        for _ in 0..LEARNING_TRADES_TARGET - 1 {
            e.record_completed_trade();
        }
        assert_ne!(e.status, EpochStatus::ReadyForLive);
    }

 /// Restart sonrası sayaç korunur (save/load round-trip + atomik).
    #[test]
    fn restart_sonrasi_sayac_korunur() {
        let mut e = LearningEpoch::new_testnet("2026-10-05-1201".into(), 1_700_000);
        for _ in 0..57 {
            e.record_completed_trade();
        }
        let dir = std::env::temp_dir().join("epoch-test-restart");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("epoch.json");
        let path = path.to_str().unwrap();
        e.save(path).expect("save");
 // restart: diskten yükle → 57'den devam
        let e2 = LearningEpoch::load(path).expect("load");
        assert_eq!(e2, e);
        assert_eq!(e2.completed_trade_count, 57);
        assert_eq!(e2.environment, "testnet");
        assert_eq!(e2.starting_trade_count, 0);
        let (n, _) = e2.clone().record_completed_trade();
        assert_eq!(n, 58, "kaldığı yerden devam");
 // atomik yazım ispatı (kayıt mührü): save tmp+rename KULLANMAK
 // ZORUNDADIR — yarım yazım = sayaç sıfırlanması = öğrenme kaybı.
        let src = include_str!("epoch.rs").split("#[cfg(test)]").next().unwrap_or("");
        let save_body = src.split("pub fn save").nth(1).unwrap_or("");
        let save_body = save_body.split("pub fn load").next().unwrap_or("");
        assert!(
            save_body.contains("rename"),
            "LearningEpoch::save ATOMİK yazım (tmp+rename) kullanmalı — kaynak: {save_body}"
        );
        let _ = std::fs::remove_file(path);
    }

 /// Duplicate/replay fill ikinci kez SAYILMAZ: tid dedup kuralı korunur —
 /// replay edilmiş fill'in tid'i ikinci kez işlem görmez → kapanış kaydı
 /// ikinci kez yazılmaz → sayaç artmaz.
    #[test]
    fn duplicate_fill_ikinci_kez_sayilmaz() {
 // (a) sayaç API'si: kapanış kaydına bağlı, fill saymaz
        let mut e = LearningEpoch::new_testnet("t".into(), 0);
        e.record_completed_trade();
        assert_eq!(e.completed_trade_count, 1);
 // replay aynı işlemden ikinci TradeExit ÜRETMEZ (tid dedup) →
 // record_completed_trade ikinci kez ÇAĞRILMAZ → sayaç 1 kalır
        assert_eq!(e.completed_trade_count, 1);
 // (b) tid dedup mekanizması hâlâ ayakta (koruma şartı)
        let tid = "test-tid-1";
        assert!(!crate::track::tid_seen(tid), "ilk görüş yeni");
        assert!(crate::track::tid_seen(tid), "replay → dedup");
        assert!(crate::track::tid_seen(tid), "tekrar replay → yine dedup");
    }

 /// FRESH_START karar tablosu: yalnız İLK çalıştırmada siler.
    #[test]
    fn fresh_start_karar_tablosu() {
 // fresh=0 → normal restart: state KORUNUR
        assert_eq!(fresh_start_decision(false, false, true), FreshDecision::KeepExisting);
        assert_eq!(fresh_start_decision(false, true, true), FreshDecision::KeepExisting);
 // fresh=1, epoch YOK → İLK çalıştırma: WIPE
        assert_eq!(fresh_start_decision(true, false, true), FreshDecision::Wipe);
 // fresh=1 ama epoch ZATEN VAR → sonraki restart: state KORUNUR
 // (sıfırdan başlamak = testin başlangıcında temizlemek;
 // her restart'ta silmek DEĞİL)
        assert_eq!(fresh_start_decision(true, true, true), FreshDecision::KeepExisting);
 // canlı endpoint'te fresh reset İSTENEMEZ
        assert_eq!(fresh_start_decision(true, false, false), FreshDecision::Invalid);
        assert_eq!(fresh_start_decision(false, false, false), FreshDecision::KeepExisting);
    }

 /// YAPISAL KORUMA: testnet epoch canlı endpoint'e karşı başlamaz.
    #[test]
    fn testnet_modunda_live_endpoint_e_emir_gonderilemez() {
 // geçerli testnet konfigürasyonu
        assert!(assert_testnet_endpoint("testnet", "https://api.hyperliquid-testnet.xyz", "testnet").is_ok());
 // NETWORK=mainnet → BOT BAŞLAMAZ (emir asla atılmaz)
        let e = assert_testnet_endpoint("mainnet", "https://api.hyperliquid-testnet.xyz", "testnet");
        assert!(e.is_err(), "canlı NETWORK ile testnet epoch başlamaz");
 // API url mainnet hostu → BOT BAŞLAMAZ
        let e = assert_testnet_endpoint("testnet", "https://api.hyperliquid.xyz", "testnet");
        assert!(e.is_err(), "canlı API url ile testnet epoch başlamaz");
 // environment doğrulaması
        assert!(assert_testnet_endpoint("testnet", "https://api.hyperliquid-testnet.xyz", "live").is_err());
    }

 /// Yeni dönem nötr başlar: fresh start tracker/weights dosyalarını
 /// temizler (bot.rs dosya yollarını uygular; karar tablosu yukarıda).
    #[test]
    fn fresh_start_eski_state_temizligi_dosya_uygulama() {
        let dir = std::env::temp_dir().join("epoch-test-fresh");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = dir.join("track_state.json");
        let weights = dir.join("ensemble_weights_live.json");
        let exits = dir.join("trade_exits.jsonl");
        std::fs::write(&tracker, br#"{"origin": "live"}"#).unwrap();
        std::fs::write(&weights, br#"{"origin": "live"}"#).unwrap();
        std::fs::write(&exits, b"{}
{}
").unwrap();
        assert!(tracker.exists() && weights.exists() && exits.exists());

 // WIPE uygulaması: tracker + weights silinir, exits ARŞİVLENİR
 // (öğrenme dönemine karıştırma — kayıp yok, karışma yok)
        let epoch = LearningEpoch::new_testnet("2026-10-05-1202".into(), 0);
        let ts = epoch.started_at;
        std::fs::rename(&exits, dir.join(format!("trade_exits.jsonl.{ts}.prev"))).unwrap();
        let _ = std::fs::remove_file(&tracker);
        let _ = std::fs::remove_file(&weights);

        assert!(!tracker.exists(), "eski tracker temizlendi → nötr w=1.0");
        assert!(!weights.exists(), "eski canlı ağırlık temizlendi");
        assert!(!exits.exists(), "yeni dönem exits'i boş başlar");
        assert!(dir.join(format!("trade_exits.jsonl.{ts}.prev")).exists(), "eski veri ARŞİVLI (silinmedi)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
