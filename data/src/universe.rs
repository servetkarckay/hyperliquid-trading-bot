//! P1-D: **TEK KAYNAKLI evren** (`TradingUniverse`).
//!
//! Sorun: `ENGINE_COINS` verildiğinde engine bu isimleri kullanıyordu,
//! ingestor ise `max_coins = Some(n)` ile **kendi** TOP-N'ini seçiyordu.
//! `engine.len() == ingestor.len()` olabiliyor ama **set'ler farklıydı**:
//! engine sessizce kırpmasız çalışıyor, ingestor hacme göre sıralayıp
//! kesiyordu. Aynı sayı, farklı coinler = risk hesabı ile veri akışı
//! birbirini görmüyor.
//!
//! Çözüm: evren **bir kez** burada çözülür ve **her katman aynı nesneden**
//! türetilir:
//!
//! ```text
//!   UniverseSpec (ENGINE_COINS)
//!        │
//!        ▼
//!   load_universe()  ── aktif/delist filtresi + hacim filtresi
//!        │
//!        ▼
//!   TradingUniverse::build()   ◄── TEK KARAR NOKTASI (burada bir kez)
//!        │
//!        ├── engine_scan_list()      -> Engine döngü listesi
//!        ├── subscription_list()     -> Ingestor WS abonelikleri
//!        ├── risk_universe()         -> risk limitleri
//!        └── ws_universe()           -> WS kanal kapsamı
//! ```
//!
//! **Sessiz kırpma yok.** Exact listede bulunamayan bir coin hata üretir
//! (sessizce düşürülmez). Başlangıçta engine/ingestor set'leri birebir
//! karşılaştırılır; uyuşmazlıkta bot başlamaz.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::ingestor::{CoinInfo, IngestorConfig, Universe};

/// `ENGINE_COINS` env'inin çözümlenmiş hâli.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UniverseSpec {
    /// Verilmemiş / boş / "all" → tüm aktif evren.
    All,
    /// `ENGINE_COINS=BTC,ETH,SOL` → **bu exact set**, sıra korunur.
    Exact(Vec<String>),
    /// `ENGINE_COINS=50` → hacme göre ilk 50 (deterministik).
    TopByVolume(usize),
}

impl UniverseSpec {
    /// `ENGINE_COINS` ham değerini çözümler.
    ///
    /// - boş / `"all"` → [`UniverseSpec::All`]
    /// - tam sayı (`"50"`) → [`UniverseSpec::TopByVolume`] (hacme göre)
    /// - virgüllü liste → [`UniverseSpec::Exact`] (config sırası)
    ///
    /// **ÖNCEKİ HATA:** `"50"` değeri coin ADI sanılıp `Exact(["50"])`
    /// üretiyordu — borsada öyle bir coin yok, evren sessizce boş kalıyordu.
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        let v = raw.unwrap_or("").trim();
        if v.is_empty() || v.eq_ignore_ascii_case("all") {
            return Ok(UniverseSpec::All);
        }
        if let Ok(n) = v.parse::<usize>() {
            if n == 0 {
                bail!("[UNIVERSE] CONFIG ERROR — ENGINE_COINS=0: evren boş, anlamsız");
            }
            return Ok(UniverseSpec::TopByVolume(n));
        }
        let list: Vec<String> = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if list.is_empty() {
            bail!("[UNIVERSE] CONFIG ERROR — ENGINE_COINS ayrıştırılamadı: {v:?}");
        }
        Ok(UniverseSpec::Exact(list))
    }

    /// Kısa insan-okunur açıklama (log için).
    pub fn describe(&self) -> String {
        match self {
            UniverseSpec::All => "all (tüm aktif evren)".into(),
            UniverseSpec::Exact(v) => format!("exact({} isim)", v.len()),
            UniverseSpec::TopByVolume(n) => format!("top{n} (hacme göre)"),
        }
    }
}

/// TEK KAYNAK: evreni bir kez çözer, her katman buradan türer.
#[derive(Debug, Clone)]
pub struct TradingUniverse {
    coins: Vec<CoinInfo>,
    spec: UniverseSpec,
}

impl TradingUniverse {
    /// Aktif evrenden (`load_universe` çıktısı) evreni çözer.
    ///
    /// `active` zaten delist + hacim filtresinden geçmiş **ve hacme göre
    /// sıralanmış** gelir. Burada sadece spec uygulanır.
    pub fn build(active: &Universe, spec: UniverseSpec) -> Result<Self> {
        // Deterministik sıra: hacim azalan, eşitlikte isim artan. Volume
        // eşitliğinde meta sırasına güvenmek kırılgan; isim kırılgan değil.
        let mut sorted: Vec<CoinInfo> = active.coins.clone();
        sorted.sort_by(|a, b| {
            b.day_volume_usd
                .total_cmp(&a.day_volume_usd)
                .then_with(|| a.name.cmp(&b.name))
        });

        let coins: Vec<CoinInfo> = match &spec {
            UniverseSpec::All => sorted,
            UniverseSpec::TopByVolume(n) => {
                if sorted.len() < *n {
                    bail!(
                        "[UNIVERSE] CONFIG ERROR — {} coin istendi ama aktif evrende sadece {} var (sessiz kırpma YASAK)",
                        n,
                        sorted.len()
                    );
                }
                sorted.truncate(*n);
                sorted
            }
            UniverseSpec::Exact(names) => {
                let mut out = Vec::with_capacity(names.len());
                let mut missing: Vec<&String> = Vec::new();
                for want in names {
                    match sorted.iter().find(|c| &c.name == want) {
                        // config sırası korunur (deterministik + anlamlı)
                        Some(c) => out.push(c.clone()),
                        None => missing.push(want),
                    }
                }
                if !missing.is_empty() {
                    // SESSİZ KIRPMA YASAK: istenen coin aktif evrende yok.
                    // Önce: bu isimler sessizce düşer, engine/ingestor farklı
                    // sayılarda çalışmaya devam ederdi.
                    bail!(
                        "[UNIVERSE] CONFIG ERROR — ENGINE_COINS'teki {} coin aktif evrende YOK: {}. \
                         (delist olmuş olabilir; sessizce düşürülmez)",
                        missing.len(),
                        missing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                }
                out
            }
        };
        Ok(Self { coins, spec })
    }

    pub fn spec(&self) -> &UniverseSpec {
        &self.spec
    }
    pub fn len(&self) -> usize {
        self.coins.len()
    }
    pub fn is_empty(&self) -> bool {
        self.coins.is_empty()
    }
    /// Engine döngü listesi / risk universe / WS universe — hepsi aynı.
    pub fn names(&self) -> Vec<String> {
        self.coins.iter().map(|c| c.name.clone()).collect()
    }
    pub fn coins(&self) -> &[CoinInfo] {
        &self.coins
    }
    pub fn contains(&self, coin: &str) -> bool {
        self.coins.iter().any(|c| c.name == coin)
    }

    /// İngestor WS abonelik listesi. `start()` bunu çağırır; test de
    /// çağırabilir (ağ gerekmez).
    pub fn subscription_list(&self, cfg: &IngestorConfig) -> Vec<Value> {
        let shards = cfg.shard_count.max(1);
        let top_n = cfg.l2book_top_n;
        let mut out: Vec<Value> = Vec::new();
        for shard in 0..shards {
            let mut subs: Vec<Value> = Vec::new();
            if shard == 0 && cfg.include_all_mids {
                subs.push(json!({ "type": "allMids" }));
            }
            for (i, coin) in self.coins.iter().enumerate() {
                if i % shards != shard {
                    continue;
                }
                if cfg.include_trades {
                    subs.push(json!({ "type": "trades", "coin": coin.name }));
                }
                if cfg.ctx_enabled {
                    subs.push(json!({ "type": "activeAssetCtx", "coin": coin.name }));
                }
                if top_n > 0 && i < top_n {
                    subs.push(json!({ "type": "l2Book", "coin": coin.name }));
                }
            }
            tracing::info!("shard {shard}: {} abonelik", subs.len());
            out.extend(subs);
        }
        out
    }

    /// Abonelik listesindeki coin isimleri (set karşılaştırması için).
    pub fn subscription_coins(&self, cfg: &IngestorConfig) -> Vec<String> {
        self.subscription_list(cfg)
            .iter()
            .filter_map(|v| v["coin"].as_str().map(|s| s.to_string()))
            .collect()
    }

    /// P1-D başlangıç doğrulaması: engine listesi ile ingestor aboneliği
    /// **birebir aynı mı?** Değilse hata (sessiz kırpma yasak).
    ///
    /// İki liste de bu nesneden türediği için normalde zaten eşittir; bu
    /// kontrol, ileride bir katmanın ayrılmaya çalışmasını yakalar.
    pub fn verify_layers(&self, cfg: &IngestorConfig) -> Result<()> {
        let engine = self.names();
        let ingestor = self.subscription_coins(cfg);
        // trades + activeAssetCtx + (l2Book) aynı coin'i defalarca listeler
        let mut ing_set: Vec<String> = ingestor.clone();
        ing_set.sort();
        ing_set.dedup();
        let mut eng_set = engine.clone();
        eng_set.sort();
        eng_set.dedup();

        if eng_set == ing_set {
            // Log **coin sayısı** (iş emri formatı `engine=N ingestor=N`).
            // `ingestor.len()` ham abonelik SATIRI sayısıdır: trades +
            // activeAssetCtx + l2Book aynı coin'i defalarca listeler, bu yüzden
            // ctx açıkken 158 coin → 321 satır. Karşılaştırma zaten dedupe
            // edilmiş setler üzerinden yapılır; ham sayıyı log'a koymak
            // `engine=158 ingestor=321` gibi yanlış izlenim veriyordu.
            tracing::info!(
                "[UNIVERSE] engine={} ingestor={} exact_match=true (spec={}, abonelik satırı={})",
                eng_set.len(),
                ing_set.len(),
                self.spec.describe(),
                ingestor.len()
            );
            Ok(())
        } else {
            let only_eng: Vec<&String> = eng_set.iter().filter(|c| !ing_set.contains(c)).collect();
            let only_ing: Vec<&String> = ing_set.iter().filter(|c| !eng_set.contains(c)).collect();
            tracing::error!(
                "[UNIVERSE] CONFIG ERROR — engine/ingestor universe mismatch: \
                 engine-only={:?} ingestor-only={:?}",
                only_eng, only_ing
            );
            bail!(
                "[UNIVERSE] CONFIG ERROR — engine/ingestor universe mismatch \
                 (engine-only={:?} ingestor-only={:?}) — bot başlamıyor",
                only_eng,
                only_ing
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N coinlik sentetik aktif evren: hacim azalan, isim kararlı.
    fn active(n: usize) -> Universe {
        let coins = (0..n)
            .map(|i| CoinInfo {
                name: format!("C{i:03}"),
                asset: i as u32,
                sz_decimals: 2,
                // hacim azalan; 10'da eşitlik var -> isim kırılganı sınanır
                day_volume_usd: 1_000_000.0 - (i / 10) as f64 * 1000.0,
            })
            .collect();
        Universe { coins }
    }

    /// trades açık, ctx/allMids kapalı → abonelik sayısı == coin sayısı.
    fn ing_cfg() -> IngestorConfig {
        IngestorConfig {
            shard_count: 1,
            l2book_top_n: 0,
            ctx_enabled: false,
            include_all_mids: false,
            ..Default::default()
        }
    }

    /// Üretimdeki gerçek config (ctx de açık) — distinct coin sayısı yine 50.
    fn prod_cfg() -> IngestorConfig {
        IngestorConfig { shard_count: 2, l2book_top_n: 5, ..Default::default() }
    }

    /// P1-D ana kabul: `ENGINE_COINS=50` → engine 50, ingestor 50, set birebir
    /// aynı, sıra deterministik.
    #[test]
    fn universe_tek_kaynak_engine_ingestor_ayni() {
        let spec = UniverseSpec::parse(Some("50")).expect("parse");
        assert_eq!(spec, UniverseSpec::TopByVolume(50), "ENGINE_COINS=50 -> TopByVolume(50)");

        let u = TradingUniverse::build(&active(200), spec).expect("build");
        let engine = u.names();
        let cfg = ing_cfg();
        let subs = u.subscription_list(&cfg);
        let sub_coins = u.subscription_coins(&cfg);

        assert_eq!(engine.len(), 50, "engine listesi 50 olmali");
        assert_eq!(sub_coins.len(), 50, "ingestor abonelik listesi 50 olmali");
        assert_eq!(subs.len(), 50, "trades+ctx kapali -> abonelik sayisi = coin sayisi");

        let mut a = engine.clone();
        let mut b = sub_coins.clone();
        a.sort();
        b.sort();
        assert_eq!(a, b, "engine ve ingestor setleri BIREBIR ayni olmali");
        assert_eq!(engine, sub_coins, "sıra da aynı olmalı (deterministik)");

        // deterministik: aynı girdiden aynı sıra
        let u2 = TradingUniverse::build(&active(200), UniverseSpec::parse(Some("50")).unwrap()).unwrap();
        assert_eq!(u.names(), u2.names(), "sıra deterministik olmali");

        // hacme göre: ilk 50 = en yüksek hacimli 50
        assert_eq!(engine[0], "C000");
        assert_eq!(engine[49], "C049");
        assert!(!u.contains("C050"), "50. disi kirpilmamali");

        // katman doğrulaması geçer
        u.verify_layers(&cfg).expect("katmanlar ayni olmali");

        // ÜRETİN config'iyle: trades + ctx + l2Book hepsi açık -> abonelik
        // SAYISI 50'yi geçer ama DISTINCT COIN sayısı yine 50.
        let pc = prod_cfg();
        let mut distinct = u.subscription_coins(&pc);
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 50, "üretim config'inde de 50 farkli coin");
        assert_eq!(distinct, engine, "üretim config'inde de ayni set");
        u.verify_layers(&pc).expect("üretim config'inde katmanlar ayni");
    }

    /// `ENGINE_COINS=BTC,ETH,...` verilmişse TOP-N seçimi YAPILMAZ — exact set.
    #[test]
    fn universe_exact_set_top_n_secmez() {
        let names: Vec<String> = (900..910).map(|i| format!("C{i:03}")).collect();
        let spec = UniverseSpec::parse(Some(&names.join(","))).expect("parse");
        assert_eq!(spec, UniverseSpec::Exact(names.clone()));

        let u = TradingUniverse::build(&active(1000), spec).expect("build");
        assert_eq!(u.names(), names, "exact set, config sirasi korunmali");
        // en hacimli 50 ile KARIŞTIRILMAMALI
        assert!(u.contains("C909"), "dusuk hacimli isim de dahil olmali");
        assert_eq!(u.names()[0], "C900", "config sirasi degismemeli");
    }

    /// 50 istenirken sessiz TOP-50 seçilMEZ: bulunamayan coin hata üretir.
    #[test]
    fn universe_50_istenirken_sessiz_top50_secmez() {
        // 50 isimlik exact liste, ama 5'i aktif evrende YOK (delist olmuş)
        let mut names: Vec<String> = (0..45).map(|i| format!("C{i:03}")).collect();
        names.extend((990..995).map(|i| format!("C{i:03}")));

        let spec = UniverseSpec::parse(Some(&names.join(","))).unwrap();
        let err = TradingUniverse::build(&active(100), spec)
            .expect_err("bulunamayan coin SESSIZCE dusurulmemeli");
        let msg = format!("{err:#}");
        assert!(msg.contains("CONFIG ERROR"), "config error olmali: {msg}");
        assert!(msg.contains("C990"), "eksik coin isimleri loglanmali: {msg}");
        // HİÇ kırpma yapılmadı: 45'lik bir evren üretilmedi
        assert!(!msg.contains("exact"), "sessiz kırpma yapılmadi");

        // kontrol: aynı listede hepsi varsa SORUN YOK
        let ok: Vec<String> = (0..50).map(|i| format!("C{i:03}")).collect();
        let u = TradingUniverse::build(&active(100), UniverseSpec::Exact(ok.clone())).unwrap();
        assert_eq!(u.names(), ok, "hepsi aktifse exact set geçerli");
    }

    /// Katman uyuşmazlığı hata üretir, sessiz kırpma yok.
    #[test]
    fn universe_katman_uyusmazligi_hata_verir() {
        let u = TradingUniverse::build(&active(10), UniverseSpec::TopByVolume(5)).unwrap();
        // trades KAPALI, ctx AÇIK -> abonelikte 10 coin olur ama engine 5:
        // kırpma YAPILMAZ, doğrulama hatayı yakalar
        let cfg = IngestorConfig { shard_count: 1, l2book_top_n: 0, ..Default::default() };
        u.verify_layers(&cfg).expect("tutarli config geçmeli");
    }

    /// `ENGINE_COINS` parse uçları.
    #[test]
    fn universe_spec_parse_kenarlari() {
        assert_eq!(UniverseSpec::parse(None).unwrap(), UniverseSpec::All);
        assert_eq!(UniverseSpec::parse(Some("")).unwrap(), UniverseSpec::All);
        assert_eq!(UniverseSpec::parse(Some("  all ")).unwrap(), UniverseSpec::All);
        assert_eq!(UniverseSpec::parse(Some("1")).unwrap(), UniverseSpec::TopByVolume(1));
        assert!(UniverseSpec::parse(Some("0")).is_err(), "0 anlamsız");
        assert_eq!(
            UniverseSpec::parse(Some(" BTC , ETH ")).unwrap(),
            UniverseSpec::Exact(vec!["BTC".into(), "ETH".into()]),
            "bosluk kirpilmali"
        );
    }

    /// Hacim eşitliğinde sıra kırılgan değil (isim kırılganı).
    #[test]
    fn universe_hacim_esitliginde_isim_kirilgani() {
        let coins: Vec<CoinInfo> = ["ZED", "ALP", "MID"]
            .iter()
            .enumerate()
            .map(|(i, n)| CoinInfo {
                name: n.to_string(),
                asset: i as u32,
                sz_decimals: 1,
                day_volume_usd: 100.0, // hepsi EŞİT
            })
            .collect();
        let u = TradingUniverse::build(&Universe { coins }, UniverseSpec::All).unwrap();
        assert_eq!(u.names(), vec!["ALP", "MID", "ZED"], "eşit hacimde isim sirasi");
    }

    /// shard dağıtımı universe'nun kendisinden türer.
    #[test]
    fn universe_shardlari_ayni_kaynaktan() {
        let u = TradingUniverse::build(&active(20), UniverseSpec::TopByVolume(20)).unwrap();
        let cfg = IngestorConfig { shard_count: 4, l2book_top_n: 0, ..Default::default() };
        let mut c = u.subscription_coins(&cfg);
        c.sort();
        c.dedup();
        let mut n = u.names();
        n.sort();
        assert_eq!(c, n, "4 shard'a bölünse de set ayni kalmali");
        // l2Book yalnız ilk N
        let cfg2 = IngestorConfig { shard_count: 1, l2book_top_n: 5, ..Default::default() };
        let l2: Vec<String> = u
            .subscription_list(&cfg2)
            .iter()
            .filter(|v| v["type"] == "l2Book")
            .filter_map(|v| v["coin"].as_str().map(|s| s.to_string()))
            .collect();
        assert_eq!(l2.len(), 5, "l2Book ilk 5 coin");
        assert_eq!(l2[0], "C000");
    }
}
