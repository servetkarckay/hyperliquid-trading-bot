//! EXECUTOR — api::Exchange ince katmanı: emir durumu takibi + koruma.
//!
//! Görevler:
//! 1. `place_limit / place_ioc / cancel / cancel_all`: lot/fiyat yuvarlama,
//! $10 minimum, hayalet emir koruması.
//! 2. BORSA STOP/TP: `ensure_sl / replace_sl (trailing) / cancel_sl` ve
//! `ensure_tp / cancel_tp` — reduce-only trigger (`tpsl="tp"/"sl"`),
//! idempotent (`sl_orders`/`tp_orders` + TS_GRACE_MS grace kapısı —
//! çift emir yasağı).
//! 3. DOLUM: `handle_ws_event` (orderUpdates + user fills) → pozisyon
//! haritası + `FillEvent` (is_tp_fill/is_sl_fill oid eşleşmesiyle).
//! 4. RECONCILIATION `parse_open_orders` cloid eşleşmesi —
//! UNKNOWN emir borsada bulunursa oid bağlanır, bulunamazsa yalnız
//! oid==0 pending'de Flat revert.
//! 5. `make_cloid`: deterministik 16-hane hex (retry idempotency).
//! 6. `save/load_px_dec`: öğrenilen tick hassasiyeti kalıcılığı.
use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use hyperliquid_api::exchange_endpoint::{OrderStatuses, WireOrder};

/// Stop konulduktan sonra aynı coin için tekrar denenmeme süresi (ms).
/// WS `orderUpdates` "open" bildirimi borsada stop'un ardından geliyor ve
/// reconnect'te `sl_orders` boşalıyor; bu kapı olmadan motor aynı stop'u
/// 5 dakikada bir yeniden kuruyordu.
pub const TS_GRACE_MS: u64 = 600_000;

/// Deterministik idempotency anahtarı.
///
/// Hyperliquid cloid = **128 bit hex**, `0x` prefix'li → 32 hex hane
/// (docs: "optional 128 bit hex string, e.g. 0x1234...cdef"). 16 hane
/// gönderilince API 422 "Failed to deserialize the JSON body" döndü (testnet
/// 09:22'de Not: tüm flatten emirleri reddedildi).
///
/// Aynı (coin, fiyat, boyut, döngü) için aynı anahtar üretilir → `post`'un
/// 429/5xx retry'ı aynı emri ikinci kez yollayamaz.
pub fn make_cloid(coin: &str, px: f64, sz: f64, salt: u64) -> String {
 // FNV-1a 128 (iki bağımsız 64-bit yazmacı çiftler)
    let mut hi: u64 = 0x6c62_272e_07bb_0142;
    let mut lo: u64 = 0x62b8_2175_6295_c58d;
    let mut mix = |mut h: u64, v: u64| {
        for b in v.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        h
    };
    for b in coin.as_bytes() {
        hi = mix(hi, *b as u64);
        lo = mix(lo, (*b as u64) ^ hi);
    }
    hi = mix(hi, px.to_bits());
    lo = mix(lo, sz.to_bits());
    hi = mix(hi, salt);
    lo = mix(lo, salt.rotate_left(17) ^ hi);
    format!("0x{hi:016x}{lo:016x}")
}

use hyperliquid_api::{Config, Exchange, Info, Tif, now_ms};

/// RESTART DEVRALMA: bu dinlenen satırın cloid'i TP TUZUYLA
/// üretilenle birebir eşleşiyor mu?
///
/// `make_cloid` deterministiktir (coin+px+sz+tuz) → restart sonrası RAM'de
/// kaybolan `tp_last_px`/`tp_orders`'a GEREK KALMAZ: REST satırının
/// (px=sz'den yeniden üretilen) cloid'i TP tuzuyla aynıysa BU BİZİM
/// TP'MİZDİR. SL cloid'i farklı tuz (dec) kullandığı için çapraz eşleşmez.
pub fn cloid_is_tp(coin: &str, row_px: f64, row_sz: f64, row_cloid: Option<&str>) -> bool {
    let Some(c) = row_cloid else { return false };
    if !(row_px > 0.0) || !(row_sz > 0.0) {
        return false;
    }
    let expected = make_cloid(coin, row_px, row_sz, TP_CLOID_SALT);
    c.eq_ignore_ascii_case(&expected)
}

/// bu dinlenen reduce-only satır **bizim TP trigger'ımız mı?**
///
/// TP trigger-LIMIT'in REST `limitPx` değeri TP fiyatının KENDİSİDİR
/// (ölçek yok); `tp_last_px` ile ±%0.1 eşleşme ayırt edicidir
/// (SL trigger-MARKET'in `limitPx`'i trigger×1.05/0.95 → asla eşleşmez).
/// `None`/0 = tp_last_px bilinmiyor → "TP" denemez (SL dedupe'una gider;
/// oradaki SL-WARN seviye assert'i yine korur).
pub fn is_our_tp_row(row_px: f64, reduce_only: bool, tp_last_px: Option<f64>) -> bool {
    let Some(want) = tp_last_px else { return false };
    reduce_only && want > 0.0 && row_px > 0.0 && (row_px - want).abs() / want <= 0.001
}

/// `user` kanalından gelen gerçek dolum (borsa ort. fiyatı burada verir)
#[derive(Debug, Clone)]
pub struct FillEvent {
    pub oid: i64,
    pub coin: String,
    pub px: f64,
    pub sz: f64,
    pub closed_pnl: f64,
    pub is_buy: bool,
 /// Bu dolum **bizim TP trigger**'ımızdan mı geldi (oid eşleşmesi)?
 /// F: `ExitReason::TakeProfit` yalnız bundan atanır (MFE/analizden değil).
    pub is_tp_fill: bool,
 /// Bu dolum **bizim SL trigger**'ımızdan mı geldi? (SL/trailing ayrımı
 /// ratchet sayısıyla engine'de yapılır — borsa tarafı tek oid'i paylaşır.)
    pub is_sl_fill: bool,
}

/// BLOCKER-2 DÜZELTMESİ: dolumun açılış/kapanış ayrımı `closedPnl`'den
/// YAPILMAZ. Hyperliquid fill şeması `dir` alanını ayrıca verir:
/// "Open Long"/"Open Short" (açılış), "Close Long"/"Close Short"
/// (kapanış), "Long > Short"/"Short > Long" (ters çevirme — eski tarafı
/// kapatıp yeniyi açar → yeni taraf için açılış).
///
/// `closedPnl == 0` kesinlikle "açılış" demek değildir: break-even
/// kapanışta 0'dır ve eski kod tam bunu açılış sanıp giriş korumasını
/// TEKRAR kuruyordu.
pub fn is_open_fill(dir_field: &str) -> bool {
    dir_field.starts_with("Open") || dir_field.contains('>')
}

/// BORSA STOP grid denemesi: fiyat tick'i coin'e özel ve meta'da YOK
/// (Not: ME 6 haneyi reddediyor). "invalid price" gelince kaba ızgarada
/// bir hane azaltılır. Giriş yolu (`batch_execute`) ile aynı strateji.
async fn place_sl_grid(
    ex: &Exchange,
    asset: u32,
    exit_is_buy: bool,
    qty: f64,
    sl: f64,
    mut dec: u32,
    coin: &str,
) -> Result<i64> {
    let mut last = String::new();
    while dec >= 1 {
        let px = hyperliquid_api::round_px_for(sl, dec);
 // Cloid: `signing::post` 5xx retry'ında aynı emri tekrar yollayabilir.
 // `salt = dec` ile ızgara denemeleri birbirinden ayrılır, ama AYNI
 // ızgaranın retry'si aynı cloid'i alır → borsa ikinciyi reddeder.
        let cloid = make_cloid(coin, px, qty, dec as u64);
        match ex
            .place_trigger(asset, exit_is_buy, qty, px, true, false, true, Some(&cloid))
            .await
        {
            Ok(st) if st.error(0).is_none() => return Ok(st.resting_oid(0).unwrap_or(0)),
            Ok(st) => {
                last = st.error(0).unwrap_or("bilinmeyen").to_string();
                if !last.contains("invalid price") {
                    break;
                }
                dec -= 1;
            }
            Err(e) => {
                last = e.to_string();
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    bail!("SL reddi (hane={dec}): {last}")
}

/// **REDUCE-ONLY TP TRIGGER** — `tpsl="tp"`, `is_market=false` (dinlenen
/// limit). TP artık gerçek kapanış kaynağı: fill geldiğinde `oid` eşleşmesi
/// ile `ExitReason::TakeProfit` atanır.
///
/// SL ile AYNI ızgara mantığı: borsa fiyat ızgarasını reddederse hane
/// azaltılarak denenir; her deneme AYRI cloid tuzu (`TP_CLOID_SALT`) alır.
async fn place_tp_grid(
    ex: &Exchange,
    asset: u32,
    exit_is_buy: bool,
    qty: f64,
    tp: f64,
    mut dec: u32,
    coin: &str,
) -> Result<i64> {
    let mut last = String::new();
    while dec >= 1 {
        let px = hyperliquid_api::round_px_for(tp, dec);
        let cloid = make_cloid(coin, px, qty, TP_CLOID_SALT);
        match ex
            .place_trigger(asset, exit_is_buy, qty, px, false, true, true, Some(&cloid))
            .await
        {
            Ok(st) if st.error(0).is_none() => return Ok(st.resting_oid(0).unwrap_or(0)),
            Ok(st) => {
                last = st.error(0).unwrap_or("bilinmeyen").to_string();
                if !last.contains("invalid price") {
                    break;
                }
                dec -= 1;
            }
            Err(e) => {
                last = e.to_string();
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    bail!("TP reddi (hane={dec}): {last}")
}

#[derive(Debug, Clone)]
pub struct OrderInfo {
    pub oid: i64,
    pub coin: String,
 /// Tick çözümlemesi için (stop fiyatının `limitPx`'ten geri çevrilmesi
 /// coin'in fiyat ızgarasına oturtulmalı).
    pub asset: u32,
    pub is_buy: bool,
    pub sz: f64,
    pub px: f64,
    pub tif: Tif,
    pub reduce_only: bool,
    pub ts: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Position {
    pub coin: String,
 /// pozitif: long, negatif: short
    pub szi: f64,
    pub entry_px: f64,
    pub position_value: f64,
    pub unrealized_pnl: f64,
    pub leverage: f64,
    pub liquidation_px: Option<f64>,
}

pub struct Executor {
    ex: Exchange,
    info: Info,
 /// master (gerçek hesap) adresi — kullanıcı sorguları için
    user: String,
 /// coin -> (asset id, szDecimals, max_leverage)
    pub assets: HashMap<String, (u32, u32, u32)>,
    open_orders: HashMap<i64, OrderInfo>,
    positions: HashMap<String, Position>,
    realized_pnl: f64,
 /// coin -> biriken `closedPnl`. Pozisyon kapanınca R-çarpanı (PF tabanlı
 /// terfi) bu değerden hesaplanır; `take_closed_pnl` bir kez okuyup siler.
    closed_pnl: HashMap<String, f64>,
    fill_count: usize,
 /// kaldıracı ayarlanmış coinler (tembel, bir kez)
    lev_done: HashSet<String>,
 /// orderUpdate payload teşhis sayacı
    dbg_updates: u32,
 /// `user` kanalından gelen, makineye beslenecek dolumlar
    fills_q: Vec<FillEvent>,
 /// coin'e özel fiyat tick'i (azami hane sayısı). Meta'da YOK; borsa
 /// reddedince kaba yuvarlayıp öğreniyoruz (bkz. `px_dec_for`).
    px_dec: HashMap<u32, u32>,
 /// BORSA TARAFI STOP: coin -> reduce-only trigger SL oid'i.
 /// Bot kapanırsa/çökerse/FLATTEN dışı bir halt olursa pozisyon yine de
 /// borsada korunur. Pozisyon kapanınca borsa reduce-only trigger'ı
 /// otomatik iptal eder.
    pub sl_orders: HashMap<String, i64>,
 /// coin -> stop'un KONULDUĞU zaman (ms). `ensure_sl` idempotency'si
 /// `sl_orders`'a bakıyordu; WS reconnect'te ya da `orderUpdates`'in
 /// "open" bildirimi geç geldiğinde harita boşalıyor ve motor aynı stop'u
 /// 5 dakika sonra TEKRAR kuruyordu (Not: ALT/TURBO/BIGTIME üst üste
 /// iki oid ile kuruldu). `TS_GRACE_MS` boyunca tekrar denenmez.
    pub sl_armed_at: HashMap<String, u64>,
 /// coin -> borsada DURAN stop'un fiyatı. Restart sonrası `trail` haritası
 /// boş olduğu için stop'un seviyesi bilinmez; bu harita `openOrders`
 /// taramasından geri yüklenir. Aksi halde ilk döngüde stop **giriş
 /// seviyesine** (ATR mesafesi kadar geriye) çekilirdi → koruma zayıflar.
    pub sl_last_px: HashMap<String, f64>,
 /// Arka plandaki (spawn) stop kurulumlarının oid'ini sahibe geri taşıyan
 /// kanal. `tokio::spawn` `&mut self` yakalayamaz; bu olmadan kurulan stop
 /// `sl_orders`'a **hiç** yazılmaz ve motor aynı stop'u tekrar kurar
 /// (ölçülen çift-stop hatasının asıl kaynağı).
    pub sl_tx: tokio::sync::mpsc::UnboundedSender<(String, i64, f64)>,
    pub sl_rx: tokio::sync::mpsc::UnboundedReceiver<(String, i64, f64)>,
 /// GİRİŞ EMİRİ ATILDI, henüz dolmadı: coin -> (hedef SL fiyatı, ts_ms).
 /// Dolum anında (fill gelir gelmez) borsa stop'u kurulur.
 /// DİKKAT: `run_cycle` bitmeden WS dolumu işlenemez (ana döngü `await`
 /// sırasında `oe_rx`'i okumaz) — dolayısıyla eski kayıtlar ZAMAN
 /// DAMGASI ile temizlenmeli, "pozisyon yok" diye anında silinmemeli.
 ///
 /// ⚠️ DEĞER **RİSK MESAFESİDİR** (fiyat farkı), MUTLAK STOP FİYATI DEĞİL.
 /// Ölçülen hata: mutlak fiyat karar anındaki **defter** fiyatından
 /// hesaplanıyordu, dolum ise IOC kaymasıyla **%0.9 daha kötü** geldi:
 /// defter entry=0.0090015 → stop=0.0090015 (short, +risk)
 /// gerçek dolum =0.009083 → stop dolumun **0.9% ALTINDA**
 /// → stop zaten piyasadaydı → **1.1 saniyede tetiklendi**, −$0.21.
 /// Mesafe taşınınca stop `dolum_px ± mesafe` ile kurulur ve kayma
 /// stop'u yanlış tarafa **taşıyamaz** (kayma olsa da stop dolumun
 /// lehine kalır).
    pub pending_sl: HashMap<String, (f64, u64)>,
 /// GİRİŞ EMİRİ ATILDI, henüz dolmadı: coin -> (TP MESAFESİ px, ts_ms).
 /// Dolum anında TP = dolum ± mesafe olarak reduce-only TP trigger
 /// kurulur. Mesafe `AllocationConfig::tp_from_entry`'ten gelir 
 /// tek formül; mesafe girişten bağımsız olduğu için karar-anı değeri
 /// dolum-anına taşınabilir).
    pub pending_tp: HashMap<String, (f64, u64)>,
 /// BORSA TARAFI TP: coin -> reduce-only TP trigger oid'i (`tpsl="tp"`).
 /// TP artık **gerçek kapanış kaynağıdır** (TP fill → TakeProfit).
    pub tp_orders: HashMap<String, i64>,
 /// coin -> TP'nin KONULDUĞU zaman (ms) — `ensure_tp` idempotency
 /// kapısı (`sl_armed_at` ile aynı şema; çift TP yasağı).
    pub tp_armed_at: HashMap<String, u64>,
 /// coin -> borsada DURAN TP'nin fiyatı (restart kurtarması +
 /// `openOrders` taramasından oid geri yükleme eşleşmesi).
    pub tp_last_px: HashMap<String, f64>,
 /// Arka plandaki TP kurulumlarının oid'ini taşıyan kanal (`sl_tx` şeması).
    pub tp_tx: tokio::sync::mpsc::UnboundedSender<(String, i64, f64)>,
    pub tp_rx: tokio::sync::mpsc::UnboundedReceiver<(String, i64, f64)>,
 /// Son dolumun TP/SL oid eşleşme bayrağı (apply_fill → FillEvent taşıma).
    tp_fill_flags: HashMap<String, (bool, bool)>,
}

/// TP cloid tuzu — SL tuzundan (`dec`) AYRI; aynı coin+px+sz'de SL ve TP
/// cloid çakışmasın. Sabit sihirli değer, kaynak kodda belgeli.
pub const TP_CLOID_SALT: u64 = 0x7050_0000_0000_0001;

impl Executor {
    pub async fn new(cfg: Config) -> Result<Self> {
        let user = std::env::var("USER_ADDRESS").unwrap_or_default();
        if user.is_empty() {
            bail!("USER_ADDRESS .env'de tanimli olmali (master adres)");
        }
        let info = Info::new(cfg.clone())?;
        let meta = info.meta().await?;
        let assets: HashMap<String, (u32, u32, u32)> = meta
            .universe
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name.clone(), (i as u32, m.szDecimals, m.maxLeverage)))
            .collect();
 // Arka plandaki stop kurulumlarının oid'ini taşıyan kanal.
 // `unbounded_channel` runtime'a bağlı değildir, `new` async olsa da
 // burada güvenle kurulabilir.
        let (sl_tx, sl_rx) = tokio::sync::mpsc::unbounded_channel();
        let (tp_tx, tp_rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(Self {
            ex: Exchange::new(cfg)?,
            info,
            user: user.to_ascii_lowercase(),
            assets,
            open_orders: HashMap::new(),
            positions: HashMap::new(),
            lev_done: HashSet::new(),
            dbg_updates: 0,
            fills_q: Vec::new(),
            px_dec: HashMap::new(),
            sl_armed_at: HashMap::new(),
            sl_last_px: HashMap::new(),
            sl_tx,
            sl_rx,
            sl_orders: HashMap::new(),
            pending_sl: HashMap::new(),
            pending_tp: HashMap::new(),
            tp_orders: HashMap::new(),
            tp_armed_at: HashMap::new(),
            tp_last_px: HashMap::new(),
            tp_tx,
            tp_rx,
            tp_fill_flags: HashMap::new(),
            realized_pnl: 0.0,
            closed_pnl: HashMap::new(),
            fill_count: 0,
        })
    }

 /// `clearinghouseState.assetPositions` dizisini pozisyon haritasına çevir
 /// (saf fonksiyon — testnet'te görülemeyen "boş hesap" senaryosu test edilir).
    pub fn parse_positions(arr: &[serde_json::Value]) -> HashMap<String, Position> {
        let mut next: HashMap<String, Position> = HashMap::with_capacity(arr.len());
        for ap in arr {
            let p = &ap["position"];
            let Some(coin) = p["coin"].as_str().filter(|c| !c.is_empty()) else {
                continue;
            };
            next.insert(
                coin.to_string(),
                Position {
                    szi: p["szi"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    entry_px: p["entryPx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    position_value: p["positionValue"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0),
                    unrealized_pnl: p["unrealizedPnl"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0),
                    leverage: p["leverage"]["value"].as_u64().map(|v| v as f64).unwrap_or(1.0),
                    liquidation_px: p["liquidationPx"].as_str().and_then(|s| s.parse().ok()),
                    coin: coin.to_string(),
                },
            );
        }
        next
    }

 /// `clearinghouseState.assetPositions` dizisinden haritayı yeniden kur.
    pub fn sync_positions_from(&mut self, arr: &[serde_json::Value]) -> Vec<String> {
        let next = Self::parse_positions(arr);
 // BOŞ DİZİ = HATA DEĞİL, GERÇEKTİR.
 //
 // Yanlış ayrım çok pahalıydı: "boş dizi → hata, eski haritayı koru"
 // kuralı, tüm pozisyonlar gerçekten kapandığında (SL'ler borsada
 // tetiklendi, `user` dolumu WS kopması yüzünden kaçtı) haritayı
 // ÖMÜR BOYU DONDURUYORDU → `open_now` tavanı hiç açılmıyor, `pz`
 // yanlış, senkron hep eski haritayı "tekrar doğruluyordu".
 //
 // Doğru ayrım: `assetPositions` **anahtarı yoksa / null ise** hata
 // (parse sorunu → harita korunur). Anahtar var ve dizi boşsa hesap
 // gerçekten flat demektir, ve bu DURUMDA DOĞRU OLAN BİR ŞEY YOKTUR:
 // borsa pozisyon yok diyor, biz de kabul etmeliyiz.
 //
 // Yalnız istisna: **hiç açılmamış** hesapta yerel harita zaten boş
 // olduğu için `next.is_empty` normaldir — o ayrım zaten `positions`
 // boş olduğu için kendiliğinden geçerli.
        let gone: Vec<String> = self
            .positions
            .keys()
            .filter(|c| !next.contains_key(*c))
            .cloned()
            .collect();
        self.positions = next;
 // Pozisyonu borsada kapanmış coin'in stop kaydı da düşmeli: kalsaydı
 // sonraki döngüde "stop hâlâ emirdir" deyip yeniden kullanırdı.
        for c in gone.iter() {
            self.sl_orders.remove(c);
                self.sl_armed_at.remove(c);
            self.pending_sl.remove(c);
        }
        gone
    }

 /// Hesap kaldıracını AÇIKÇA ayarla (cross). Varsayılan 20x → likidasyon
 /// mesafesi ~%5'e iner; botun risk modeli bunu göremez.
 ///
 /// **Bloklamaz ve kilitlemez:** coinler 8'erli yığınlarda paralel işlenir,
 /// her istekte 8 sn zaman aşımı. 178 coin'lik evrende sıralı çağrı ~4 dk
 /// sürüyor ve açılışta motor döngüsünü kilitliyordu.
    pub async fn set_leverage_all(&mut self, coins: &[String], leverage: u32) -> Vec<String> {
        let mut failed = Vec::new();
        const CHUNK: usize = 8;
        for chunk in coins.chunks(CHUNK) {
            let mut jobs = Vec::new();
            for c in chunk {
                let Ok((asset, _)) = self.asset_of(c) else {
                    failed.push(format!("{c}: bilinmeyen coin"));
                    continue;
                };
                let ex = self.ex.clone();
                let coin = c.clone();
                jobs.push(tokio::spawn(async move {
                    let fut = async move {
                        tokio::time::timeout(
                            std::time::Duration::from_secs(8),
                            ex.update_leverage(asset, true, leverage),
                        )
                        .await
                    };
                    match fut.await {
                        Ok(Ok(v)) => {
                            let ok = v
                                .get("response")
                                .and_then(|r| r.get("type"))
                                .and_then(|t| t.as_str())
                                == Some("ok");
                            if !ok {
                                Some(format!("{coin}: {v}"))
                            } else {
                                None
                            }
                        }
                        Ok(Err(e)) => Some(format!("{coin}: {e}")),
                        Err(_) => Some(format!("{coin}: zaman aşımı (8 sn)")),
                    }
                }));
            }
            for j in jobs {
                if let Ok(Some(msg)) = j.await {
                    failed.push(msg);
                }
            }
        }
        failed
    }

 /// Kaldıraç **tembel** ayarlanır: yalnız gerçekten pozisyon açılacak coin
 /// için, ilk emrinden hemen önce, bir kez. Açılışta 178 paralel imzalı
 /// istek atmak borsanın hız sınırını (HTTP 429) patlatıyordu.
    pub async fn ensure_leverage(&mut self, coin: &str, leverage: u32) -> Result<()> {
        if self.lev_done.contains(coin) {
            return Ok(());
        }
        let (asset, _) = self.asset_of(coin)?;
 // Borsa coin'in kendi max kaldıracını reddediyor ("Invalid leverage
 // value") → meta'daki maxLeverage ile sınırlayıp en az 1x yap.
        let lev = match self.max_leverage_of(coin) {
            Some(m) => leverage.min(m).max(1),
            None => leverage,
        };
        let v = self.ex.update_leverage(asset, true, lev).await?;
        let good = v
            .get("response")
            .and_then(|r| r.get("type"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| t == "ok" || t == "default");
        if !good {
            tracing::warn!("{coin}: kaldıraç reddedildi {v}");
            return Ok(()); // emir yine de gitsin; emir değeri kaldıraçtan bağımsız
        }
        self.lev_done.insert(coin.to_string());
        tracing::info!("{coin}: kaldıraç {lev}x cross ayarlandı");
        Ok(())
    }

 /// coin'in geçerli fiyat hanesi (keşfedilmişse) — varsayılan 6
    fn px_dec_for(&self, asset: u32) -> u32 {
        self.px_dec.get(&asset).copied().unwrap_or(6)
    }

 /// Öğrenilen tick hassasiyetini diske yaz (restart'ta unutulmasın).
 /// Coin'in tick'i meta'da YOK; "invalid price" reddedince ızgara yürüyerek
 /// bulunuyor. Öğrenme süreci her açılışta tekrar "invalid price" faturası
 /// demek (kısmen `open_orders` taramasından da yeniden kuruluyor).
    pub fn save_px_dec(&self, path: &str) -> std::io::Result<()> {
        if self.px_dec.is_empty() {
            return Ok(());
        }
        let map: std::collections::BTreeMap<String, u32> = self
            .assets
            .iter()
            .filter_map(|(name, (a, _, _))| self.px_dec.get(a).map(|d| (name.clone(), *d)))
            .collect();
        if let Some(p) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::write(path, serde_json::to_string(&map).unwrap_or_default())
    }

    pub fn load_px_dec(&mut self, path: &str) {
        let Ok(s) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(map) = serde_json::from_str::<HashMap<String, u32>>(&s) else {
            return;
        };
        let mut n = 0;
        for (name, d) in map {
            if let Some((a, _, _)) = self.assets.get(&name) {
                self.px_dec.insert(*a, d);
                n += 1;
            }
        }
        if n > 0 {
            tracing::info!("[TICK] {n} coin'in tick hassasiyeti diskten geri yüklendi");
        }
    }

    fn asset_of(&self, coin: &str) -> Result<(u32, u32)> {
        self.assets
            .get(coin)
            .map(|(a, d, _)| (*a, *d))
            .ok_or_else(|| anyhow!("bilinmeyen coin: {coin}"))
    }

 /// coin'in borsa tarafından izin verilen max kaldıracı
    fn max_leverage_of(&self, coin: &str) -> Option<u32> {
        self.assets.get(coin).map(|(_, _, m)| *m)
    }

 /// TÜM POZİSYONLARI KAPAT (testnet sıfırlama / temiz restart).
 ///
 /// `mids` = coin -> orta fiyat (yoksa giriş fiyatı kullanılır). Her pozisyon
 /// için ters yönde IOC + reduce_only emir atar; kalan pozisyonları sonuçta
 /// döndürür ( çağıran doğrular).
    pub async fn flatten_all(&mut self, mids: &HashMap<String, f64>) -> Vec<(String, f64)> {
        if let Err(e) = self.refresh_positions().await {
 // konum haritası bozulmadı (refresh_positions null'da korur)
            tracing::error!("[FLAT] pozisyon sorgusu başarısız ({e}) — eldeki harita kullanılıyor");
        }
        let targets: Vec<(String, f64)> = self
            .positions
            .iter()
            .filter(|(_, p)| p.szi.abs() > 1e-12)
            .map(|(c, p)| (c.clone(), p.szi))
            .collect();
        if targets.is_empty() {
            tracing::info!("[FLAT] açık pozisyon yok");
            return Vec::new();
        }
        tracing::info!("[FLAT] {} pozisyon kapatılıyor", targets.len());
        for (coin, szi) in &targets {
            let sz = szi.abs();
            let base = mids
                .get(coin)
                .copied()
                .filter(|x| *x > 0.0)
                .unwrap_or_else(|| self.positions.get(coin).map(|p| p.entry_px).unwrap_or(0.0));
            if !(base > 0.0) {
                tracing::warn!("[FLAT] {coin}: fiyat yok, atlandı");
                continue;
            }
            let is_buy = *szi < 0.0;
            let px = if is_buy { base * 1.01 } else { base * 0.99 };
 // NOT: `place_limit` adaptif fiyat tick keşfini KULLANMIYOR — ME gibi
 // 6 haneyi reddeden coin'lerde "invalid price" ile kapanıyordu
 // (Not: 4 kapakta da başarısız, pozisyon kaldı). Aynı, girişlerde
 // kanıtlanmış `batch_execute` yolunu kullan.
            if let Some((asset, szd)) = self.cached_asset(coin) {
                let qty = hyperliquid_api::round_sz_up(sz, szd);
                let w = hyperliquid_api::exchange_endpoint::WireOrder {
                    asset,
                    is_buy,
                    sz: qty,
                    px,
                    tif: Tif::Ioc,
                    reduce_only: true,
 // 5xx retry çift kapatma emri yaratmasın
                    cloid: Some(make_cloid(coin, px, qty, 0)),
                };
                match self.batch_execute(&[w], &[]).await {
                    Ok(res) => {
                        let e = res.first().and_then(|r| r.error.clone());
                        if e.is_none() {
                            tracing::info!("[FLAT] {coin}: kapatma emri gönderildi px={px:.8}");
                        } else {
                            tracing::warn!("[FLAT] {coin}: reddedildi ({e:?})");
                        }
                    }
                    Err(e) => tracing::warn!("[FLAT] {coin}: hata {e}"),
                }
            } else {
                tracing::warn!("[FLAT] {coin}: asset cache'te yok, atlandı");
            }
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        }
 // 2. GEÇİŞ: kalanları GENİŞLEYEN kapakla yeniden dene. Testnet
 // defteri ince; %1'lik kapak PUMP'ta eşleşmedi ("could not
 // immediately match"). %3 ve %6 ile tekrar.
        for round in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
            if let Err(e) = self.refresh_positions().await {
                tracing::error!("[FLAT] doğrulama sorgusu başarısız: {e}");
                break;
            }
            let left: Vec<(String, f64)> = self
                .positions
                .iter()
                .filter(|(_, p)| p.szi.abs() > 1e-12)
                .map(|(c, p)| (c.clone(), p.szi))
                .collect();
            if left.is_empty() {
                break;
            }
            let wide = 1.0 + 0.03 * (round as f64 + 1.0);
            tracing::info!("[FLAT] {} pozisyon için %{:.0} kapakla tekrar", left.len(), (wide - 1.0) * 100.0);
            for (coin, szi) in &left {
                let is_buy = *szi < 0.0;
                let base = mids.get(coin).copied().filter(|x| *x > 0.0).unwrap_or_else(|| {
                    self.positions.get(coin).map(|p| p.entry_px).unwrap_or(0.0)
                });
                if !(base > 0.0) {
                    continue;
                }
                let px = if is_buy { base * wide } else { base / wide };
                if let Some((asset, szd)) = self.cached_asset(coin) {
                    let qty = hyperliquid_api::round_sz_up(szi.abs(), szd);
                    let w = hyperliquid_api::exchange_endpoint::WireOrder {
                        asset,
                        is_buy,
                        sz: qty,
                        px,
                        tif: Tif::Ioc,
                        reduce_only: true,
                        cloid: Some(make_cloid(coin, px, qty, 1)),
                    };
                    match self.batch_execute(&[w], &[]).await {
                        Ok(res) => {
                            let e = res.first().and_then(|r| r.error.clone());
                            if e.is_none() {
                                tracing::info!("[FLAT] {coin}: tekrar gönderildi px={px:.8}");
                            } else {
                                tracing::warn!("[FLAT] {coin}: tekrar reddi ({e:?})");
                            }
                        }
                        Err(e) => tracing::warn!("[FLAT] {coin}: tekrar hata {e}"),
                    }
                } else {
                    tracing::warn!("[FLAT] {coin}: asset cache'te yok, atlandı");
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let _ = self.refresh_positions().await;
        let left: Vec<(String, f64)> = self
            .positions
            .iter()
            .filter(|(_, p)| p.szi.abs() > 1e-12)
            .map(|(c, p)| (c.clone(), p.szi))
            .collect();
        if left.is_empty() {
            tracing::info!("[FLAT] hesap temiz: tüm pozisyonlar kapandı");
        } else {
            tracing::warn!(
                "[FLAT] {} pozisyon KALDI: {}",
                left.len(),
                left.iter()
                    .map(|(c, s)| format!("{c}={s}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        left
    }

 /// BORSA TARAFI STOP-LOSS (reduce-only trigger).
 ///
 /// EN ÖLÜMCÜL KOMBİNASYON KORUMASI: kill-switch (5 hata üst üste) +
 /// borsada SL yok = açık pozisyonlar sahipsiz kalır. Bu emir bot
 /// ölse de borsada durur. reduce-only olduğu için pozisyonu tersine
 /// çeviremez, sadece kapatır.
 /// `Ok(true)` = bu çağrı **yeni** stop kurdu, `Ok(false)` = zaten vardı
 /// (idempotent, her döngü çağrılabilir).
 ///
 /// Dönüş değeri neden önemli: çağıran taraf stop'un **gerçekten**
 /// değişip değişmediğini bilmeli. `Ok` her ikisini de ifade ederdi ve
 /// çağıran, trailing'in yeni taşıdığı seviyeyi bir sonraki satırda
 /// **sabit giriş stop'una geri yazıyordu** — yani trailing her döngü
 /// kendini sıfırlıyordu.
    pub async fn ensure_sl(&mut self, coin: &str, is_long: bool, sz: f64, sl_px: f64) -> Result<bool> {
        if self.sl_orders.contains_key(coin) {
            return Ok(false); // zaten var
        }
 // SÜRE KAPISI: stop kısa süre önce kurulduysa tekrar deneme. WS
 // "open" bildirimi geç geliyor / reconnect'te `sl_orders` boşalıyor;
 // bu kapı olmadan aynı pozisyona ikinci stop asılıyordu.
        if let Some(&ts) = self.sl_armed_at.get(coin) {
            let age = (hyperliquid_api::now_ms() as i64 - ts as i64).unsigned_abs();
            if age < TS_GRACE_MS {
                return Ok(false);
            }
        }
        let Some((asset, szd)) = self.cached_asset(coin) else {
            bail!("{coin}: asset cache'te yok");
        };
        let qty = hyperliquid_api::round_sz_up(sz.abs(), szd);
        if !(qty > 0.0) || !(sl_px > 0.0) {
            return Ok(false); // hayalet koruması: 0-lot stop KONULMAZ
        }
        let is_buy = !is_long; // long -> satışta kapat
        let dec = self.px_dec.get(&asset).copied().unwrap_or(6);
        let ex = self.ex.clone();
        match place_sl_grid(&ex, asset, is_buy, qty, sl_px, dec, coin).await {
            Ok(oid) => {
                self.sl_orders.insert(coin.to_string(), oid);
                self.sl_armed_at.insert(coin.to_string(), hyperliquid_api::now_ms());
                self.sl_last_px.insert(coin.to_string(), sl_px);
                tracing::info!(
                    "[SL] {coin} borsa stop {} oid={oid} px={sl_px:.6} sz={qty} (hane={dec})",
                    if is_buy { "AL" } else { "SAT" }
                );
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

 /// **REDUCE-ONLY TP TRIGGER KUR** (`tpsl="tp"`, `is_market=false`).
 ///
 /// Idempotency (çift TP yasağı — SL ile aynı şema):
 /// 1. `tp_orders`'da oid var → kur, tekrar ASMA,
 /// 2. kısa süre önce kurulduysa (`tp_armed_at` + grace) → tekrar deneme,
 /// 3. lot 0 / fiyat 0 → hayalet koruması, emir ATILMAZ.
 ///
 /// `reduce_only = true` ZORUNLU (pozisyon büyütemez). Long TP > entry,
 /// short TP < entry — kontrol `tp_from_entry`'te (tek formül).
    pub async fn ensure_tp(&mut self, coin: &str, is_long: bool, sz: f64, tp_px: f64) -> Result<bool> {
        if self.tp_orders.contains_key(coin) {
            return Ok(false); // zaten var — körlemesine duplicate YOK
        }
        if let Some(&ts) = self.tp_armed_at.get(coin) {
            let age = (hyperliquid_api::now_ms() as i64 - ts as i64).unsigned_abs();
            if age < TS_GRACE_MS {
                return Ok(false);
            }
        }
        let Some((asset, szd)) = self.cached_asset(coin) else {
            bail!("{coin}: asset cache'te yok");
        };
        let qty = hyperliquid_api::round_sz_up(sz.abs(), szd);
        if !(qty > 0.0) || !(tp_px > 0.0) {
            return Ok(false); // hayalet koruması: 0-lot TP KONULMAZ
        }
        let exit_is_buy = !is_long; // long → TP = SAT (kâr tarafında)
        let dec = self.px_dec.get(&asset).copied().unwrap_or(6);
        let ex = self.ex.clone();
        match place_tp_grid(&ex, asset, exit_is_buy, qty, tp_px, dec, coin).await {
            Ok(oid) => {
                self.tp_orders.insert(coin.to_string(), oid);
                self.tp_armed_at.insert(coin.to_string(), hyperliquid_api::now_ms());
                self.tp_last_px.insert(coin.to_string(), tp_px);
                tracing::info!(
                    "[TP] {coin} borsa TP kuruldu oid={oid} px={tp_px:.6} sz={qty} reduce_only=true (hane={dec})"
                );
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

 /// TP trigger'ı iptal et (pozisyon kapanınca / SL dolduğunda).
    pub async fn cancel_tp(&mut self, coin: &str) {
        self.tp_armed_at.remove(coin);
        if let Some(oid) = self.tp_orders.remove(coin) {
            let _ = self.cancel(coin, oid).await;
            tracing::info!("[TP] {coin} borsa TP iptal edildi oid={oid}");
        }
    }

 /// **TP OID GERİ YÜKLEME** (restart / WS kopması sonrası).
 ///
 /// Kural (körlemesine duplicate etme yasağı): `openOrders` taramasında
 /// AYNI coin için reduce-only + fiyatı `tp_px` ile ±%0.1 eşleşen emir
 /// varsa → **bizim TP'mizdir**, oid kaydedilir, YENİ EMİR ATILMAZ.
 /// Eşleşmeyen reduce-only emirlere dokunulmaz (SL taraması onları
 /// ele alır).
    pub fn restore_tp_orders(
        &mut self,
        rows: &[OpenOrderRow],
        tp_px_by_coin: &HashMap<String, f64>,
    ) -> usize {
        let mut n = 0usize;
        for row in rows {
            if !row.reduce_only || self.tp_orders.contains_key(&row.coin) {
                continue;
            }
 // ÖNCE cloid tuzu (RAM'siz, EntryAttr'a bağımsız) —
 // restart sonrası tp_px_by_coin BOŞTUR (EntryAttr RAM'de kalır).
            if cloid_is_tp(&row.coin, row.px, row.sz, row.cloid.as_deref()) {
                self.tp_orders.insert(row.coin.clone(), row.oid);
                if row.px > 0.0 {
                    self.tp_last_px.insert(row.coin.clone(), row.px);
                }
                n += 1;
                tracing::info!(
                    "[TP] {c} TP oid BORSADAN GERİ YÜKLENDİ (oid={o} px={px:.6}) — yeni emir atılmadı",
                    c = row.coin, o = row.oid, px = row.px
                );
                continue;
            }
            let Some(&want) = tp_px_by_coin.get(&row.coin) else { continue };
            if want > 0.0 && (row.px - want).abs() / want <= 0.001 {
                self.tp_orders.insert(row.coin.clone(), row.oid);
                self.tp_last_px.insert(row.coin.clone(), want);
                n += 1;
                tracing::info!(
                    "[TP] {coin} TP oid GERİ YÜKLENDİ (oid={oid} px={px:.6}) — yeni emir atılmadı",
                    coin = row.coin, oid = row.oid, px = row.px
                );
            }
        }
        n
    }

 /// Arka planda kurulan TP'lerin oid'ini kayda al (`drain_sl_results` şeması).
    pub fn drain_tp_results(&mut self) -> usize {
        let mut n = 0usize;
        while let Ok((coin, oid, px)) = self.tp_rx.try_recv() {
            self.tp_orders.insert(coin.clone(), oid);
            if px > 0.0 {
                self.tp_last_px.insert(coin, px);
            }
            n += 1;
        }
        if n > 0 {
            tracing::info!("[TP] {n} arka-plan TP kaydı tamamlandı (tp_orders)");
        }
        n
    }

 /// **TRAILING STOP YENİLEME — KURALLI SIRA: ÖNCE YENİ STOP, SONRA ESKİYİ İPTAL.**
 ///
 /// Neden bu sira zorunlu: ters sırada (`iptal → yerleştir`) yerleştirme
 /// başarısız olursa ya da ağ koparsa pozisyon **sıfır korumasız** kalır.
 /// Not: `place_trigger` bazı coin'lerde fiyat ızgarası bulamayınca
 /// ("invalid price") denemelerde tamamen başarısız oluyor (ME: 4 deneme).
 /// Doğru sırada, yerleştirme başarısızsa eski stop **ayakta kalır** →
 /// en kötü senaryo "trailing gerçekleşmedi", "korumasız pozisyon" değil.
 ///
 /// `grace` kapısı **bilinçli olarak atlanır**: bu bir *ikinci* stop değil,
 /// *aynı stop'un* yeni seviyeye taşınmasıdır.
    pub async fn replace_sl(
        &mut self,
        coin: &str,
        is_long: bool,
        sz: f64,
        new_px: f64,
    ) -> Result<i64> {
        let old_oid = self.sl_orders.get(coin).copied();
        let Some((asset, szd)) = self.cached_asset(coin) else {
            bail!("{coin}: asset cache'te yok");
        };
        let qty = hyperliquid_api::round_sz_up(sz.abs(), szd);
        if !(qty > 0.0) || !(new_px > 0.0) {
            bail!("{coin}: geçersiz trailing stop (sz={qty} px={new_px})");
        }
        let dec = self.px_dec.get(&asset).copied().unwrap_or(6);
        let ex = self.ex.clone();
 // 1) YENİYİ KUR
        let new_oid = place_sl_grid(&ex, asset, !is_long, qty, new_px, dec, coin).await?;
 // 2) Yalnız kurulduktan SONRA eskiyi iptal et
        if let Some(o) = old_oid {
            if o != new_oid {
                match self.cancel(coin, o).await {
                    Ok(()) => {
                        self.open_orders.remove(&o);
                        tracing::info!("[SL] {coin} eski stop iptal edildi oid={o} (yeni={new_oid})");
                    }
                    Err(e) => tracing::warn!(
                        coin, oid = o, "eski stop iptal edilemedi — İKİ STOP AÇIK KALABİLİR: {e}"
                    ),
                }
            }
        }
        self.sl_orders.insert(coin.to_string(), new_oid);
        self.sl_armed_at.insert(coin.to_string(), hyperliquid_api::now_ms());
        self.sl_last_px.insert(coin.to_string(), new_px);
        tracing::info!(
            "[SL] {coin} TRAILING stop taşındı oid={new_oid} px={new_px:.6} sz={qty} (hane={dec})"
        );
        Ok(new_oid)
    }

 /// Borsa stop'unu iptal et (pozisyon kapanınca).
    pub async fn cancel_sl(&mut self, coin: &str) {
        self.sl_armed_at.remove(coin);
        if let Some(oid) = self.sl_orders.remove(coin) {
            let _ = self.cancel(coin, oid).await;
            tracing::info!("[SL] {coin} borsa stop iptal edildi oid={oid}");
        }
    }

 /// Limit emir gönder; tick/lot kurallarını uygular, durum takibine ekler.
 /// Dönen OrderInfo resting ise açık emir defterine, filled ise takibe işlenir.
    pub async fn place_limit(
        &mut self,
        coin: &str,
        is_buy: bool,
        sz: f64,
        limit_px: f64,
        tif: Tif,
        reduce_only: bool,
    ) -> Result<Option<OrderInfo>> {
 // GHOST ORDER koruması: 0.0 veya anlamsız lot borsaya GİTMEZ.
        if !sz.is_finite() || sz <= 0.0 {
            tracing::warn!("[executor] lot=0 — emir iptal (hayalet): {coin}");
            return Ok(None);
        }
        if !limit_px.is_finite() || limit_px <= 0.0 {
            tracing::warn!("[executor] fiyat geçersiz ({limit_px}) — emir iptal: {coin}");
            return Ok(None);
        }
 // borsa minimumu: $10 altı reddedilir, API çağrısı YAPILMAZ
        if !reduce_only {
            let notional = sz * limit_px;
            if notional < crate::allocation_manager::MIN_NOTIONAL_USD {
                tracing::warn!(
                    "[executor] tahsis çok küçük (${notional:.2}) — {coin} atlandı"
                );
                return Ok(None);
            }
        }
        let (asset, sz_decimals) = self.asset_of(coin)?;
        if sz <= 0.0 {
            bail!("boyut pozitif olmalı");
        }
 // lot: szDecimals'a yuvarla; notional min $10 (reduce_only hariç)
        let sz = (sz * 10f64.powi(sz_decimals as i32)).round() / 10f64.powi(sz_decimals as i32);
        let notional = sz * limit_px;
        if !reduce_only && notional < 10.0 {
            bail!("emir değeri ${notional:.2} — minimum $10");
        }

        let statuses: OrderStatuses = self
            .ex
            .place_limit(asset, is_buy, sz, limit_px, tif, reduce_only)
            .await?;
        if let Some(err) = statuses.error(0) {
            bail!("emir reddedildi: {err}");
        }
        let info = OrderInfo {
            oid: statuses.resting_oid(0).unwrap_or_else(|| {
                statuses.filled(0).map(|(_, _, oid)| oid).unwrap_or(0)
            }),
            coin: coin.to_string(),
            asset: self.cached_asset(coin).map(|(a, _)| a).unwrap_or(0),
            is_buy,
            sz,
            px: limit_px,
            tif,
            reduce_only,
            ts: now_ms(),
        };
        match (statuses.resting_oid(0), statuses.filled(0)) {
            (Some(_), _) => {
                self.open_orders.insert(info.oid, info.clone());
            }
            (None, Some((total_sz, avg_px, _))) => {
                tracing::info!(coin, total_sz, avg_px, "emir anında doldu");
                self.fill_count += 1;
            }
            _ => {}
        }
        Ok(Some(info))
    }

 /// oid ile iptal; açık emir defterinden düşer.
    pub async fn cancel(&mut self, coin: &str, oid: i64) -> Result<()> {
        let (asset, _) = self.asset_of(coin)?;
        let res = self.ex.cancel(asset, oid).await?;
        if let Some(err) = res.error(0) {
            bail!("iptal başarısız: {err}");
        }
        self.open_orders.remove(&oid);
        Ok(())
    }

 /// hesaptaki TÜM açık emirleri iptal et (fresh-start temizliği).
 ///
 /// `cancel_all(coin)` coin bazlıdır; fresh start HER coinin emirlerini
 /// temizlemelidir. Kaynak: `openOrders` taraması (bot'un kendi
 /// emirleri + trigger'lar) — her oid için tek POST, hata loglanır.
    pub async fn cancel_all_open_orders(&mut self) -> anyhow::Result<usize> {
        let rows = self.fetch_open_order_rows().await?;
        let mut n = 0usize;
        for row in &rows {
            match self.cancel(&row.coin, row.oid).await {
                Ok(()) => n += 1,
                Err(e) => tracing::warn!("[RECON] iptal hatası coin={} oid={}: {e}", row.coin, row.oid),
            }
        }
        Ok(n)
    }

 /// Coin'deki tüm açık emirleri iptal et -> iptal edilen sayı
    pub async fn cancel_all(&mut self, coin: &str) -> Result<usize> {
        self.refresh_orders().await?;
        let oids: Vec<i64> = self
            .open_orders
            .values()
            .filter(|o| o.coin == coin)
            .map(|o| o.oid)
            .collect();
        let mut n = 0;
        for oid in oids {
            match self.cancel(coin, oid).await {
                Ok(()) => n += 1,
                Err(e) => tracing::warn!(coin, oid, "iptal hatası: {e}"),
            }
        }
        Ok(n)
    }

 /// REST'ten açık emirleri yenile (ws kopması sonrası toparlama).
 /// `openOrders` yanıtını AYRIŞTIRIR ve `open_orders` cache'ini doldurur.
 /// `refresh_orders` (stop adayları) ve `refresh_open_orders_all`
 /// (reconciliation) aynı parser'ı kullanır — yeniden yazım yok.
    fn parse_open_orders(&mut self, arr: &[serde_json::Value]) -> Vec<OpenOrderRow> {
        let mut rows = Vec::new();
        self.open_orders.clear();
        for o in arr {
            let oid = match o["oid"].as_i64() {
                Some(x) => x,
                None => continue,
            };
            let cn = o["coin"].as_str().unwrap_or("").to_string();
            let asset_id = self.assets.get(&cn).map(|(a, _, _)| *a).unwrap_or(0);
            let is_buy = o["side"] == "B";
            let px = o["limitPx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let reduce_only = o["reduceOnly"].as_bool().unwrap_or(false);
 // reconciliation EMIR cloid'i üzerinden eşleşir. REST
 // `openOrders` cloid'i `cloid` alanında döner.
            let cloid = o["cloid"].as_str().map(|s| s.to_string());
            self.open_orders.insert(
                oid,
                OrderInfo {
                    oid,
                    asset: asset_id,
                    coin: cn.clone(),
                    is_buy,
                    sz: o["origSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    px,
                    tif: Tif::Gtc,
                    reduce_only,
                    ts: o["timestamp"].as_u64().unwrap_or(0),
                },
            );
 // satır sz'i = origSz (borsadaki gerçek lot) — cloid tuzu
 // tanıması (coin+px+sz+tuz deterministiktir) origSz'ye ihtiyaç duyar.
            let row_sz = o["origSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            rows.push(OpenOrderRow { coin: cn, oid, cloid, is_buy, sz: row_sz, px, reduce_only });
        }
        rows
    }

 /// TÜM dinlenen açık emirleri `(coin, oid, cloid, reduce_only, ...)`
 /// olarak döndürür.
 ///
 /// Neden ayrı API: [`Self::refresh_orders`] YALNIZ reduce-only (stop)
 /// emirleri döndürür. UNKNOWN olan **giriş** emirleri `reduce_only=false`
 /// olduğu için o listede BULUNAMAZ — yani reconciliation hiçbir zaman
 /// giriş emrini bulamazdı.
    pub async fn refresh_open_orders_all(&mut self) -> Result<Vec<OpenOrderRow>> {
        let v = self.info.open_orders(&self.user).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        Ok(self.parse_open_orders(&arr))
    }

 /// Duran **reduce-only** emirleri `(coin, oid)` olarak döndürür — bunlar
 /// borsa stop'larıdır (`dedupe_sl` ile `sl_orders`'a yazılır).
 /// Ham satırlar — TP oid geri yükleme (`restore_tp_orders`) bu satırları
 /// okur; `refresh_orders` YALNIZ SL tanıma mantığıyla süzüp döndürür.
    pub async fn fetch_open_order_rows(&mut self) -> Result<Vec<OpenOrderRow>> {
        let v = self.info.open_orders(&self.user).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        Ok(self.parse_open_orders(&arr))
    }

    pub async fn refresh_orders(&mut self) -> Result<Vec<(String, i64)>> {
        let v = self.info.open_orders(&self.user).await?;
        let arr = v.as_array().cloned().unwrap_or_default();
        let mut candidates: Vec<(String, i64)> = Vec::new();
        let rows = self.parse_open_orders(&arr);
        for row in rows {
            let (cn, oid) = (row.coin.clone(), row.oid);
 // BORSA STOP'LARINI YENİDEN TANI — ÖLÇÜLMÜŞ HATA BURADA:
 //
 // Önceki tespit `isTrigger || triggerPx` alanlarına bakıyordu ve
 // **daima false** dönüyordu. Testnet `openOrders` yanıtı bu alanları
 // **hiç göndermiyor** (Not: 09:5x, ham yanıt):
 // {'coin':'GMT','side':'B','limitPx':'0.009562','sz':'5539.0',
 // 'oid':61642165356,'reduceOnly':True,'isTrigger':None,'triggerPx':None}
 // Yani REST taraması stop'ları ASLA tanımıyordu → `sl_orders`
 // hiç dolmuyordu → `sync_exchange_sl` her döngü "eksik stop" sanıp
 // `ensure_sl`'ı çağırıyor → aynı pozisyona **ikinci** stop asılıyordu
 // (Not: GMT oid=…61867 ve …165356, LAYER …61871 ve …166722
 // aynı anda borsada duruyor).
 //
 // DOĞRU KURAL: **dinlenen (resting) + `reduceOnly` emir stop'tur.**
 // Gerekçe: bot'un diğer tüm reduce-only emirleri **IOC**'dur
 // (`flatten_all`, `Action::ExitNow` → `tif: Tif::Ioc`), IOC hiç
 // dinlenmez → `openOrders`'a girmez. Giriş emirleri de
 // `reduce_only: false`'dur. Yani defterde kalan tek reduce-only
 // emir sınıfı stop'tur.
            let reduce_only = row.reduce_only;
            let is_buy = row.is_buy;
            let px = row.px;
 // **BİZİM TP'MİZ SL DEĞİLDİR**: reduce-only trigger-LIMIT
 // TP de dinlenir; eski kural ("resting + reduceOnly = stop") TP'yi
 // de aday sayıp dedupe EN YENİ oid'i tuttuğu için oid sırasına
 // göre ya TP'yi ya SL'yi İPTAL EDİYORDU (Not: ZRO/AVAX/ONDO
 // TP'leri, BIO'nun SL'i iptal edildi).
 //
 // RESTART DEVRALMA: `tp_last_px` RAM'de kaybolur;
 // cloid tuzu deterministiktir → RAM'siz tanıma + oid GERİ YÜKLEME
 // (körlemesine ikinci TP atma yasağı da buradan kapanır).
            let row_tp = is_our_tp_row(px, reduce_only, self.tp_last_px.get(&cn).copied())
                || cloid_is_tp(&cn, px, row.sz, row.cloid.as_deref());
            if row_tp {
                if reduce_only && self.tp_orders.get(&cn) != Some(&oid) {
                    self.tp_orders.insert(cn.clone(), oid);
                    if px > 0.0 {
                        self.tp_last_px.insert(cn.clone(), px);
                    }
                    tracing::info!(
                        "[TP] {cn} TP oid BORSADAN GERİ YÜKLENDİ (oid={oid}) — yeni emir ATILMADI"
                    );
                }
                continue;
            }
            let asset_id = self.assets.get(&cn).map(|(a, _, _)| *a).unwrap_or(0);
            if reduce_only {
                let has_pos = self
                    .positions
                    .get(&cn)
                    .map(|p| p.szi.abs() > 1e-12)
                    .unwrap_or(false);
                if !cn.is_empty() && has_pos {
 // AYNI COİN İÇİN BİRDEN FAZLA STOP = ARTIK. Çözüm
 // `dedupe_sl`'de: en yeni oid tutulur, eskileri iptal edilir.
 //
 // **TUTARLILIK ASSERT'İ**. `sl_orders[coin].px`'ye
 // (yani `sl_last_px`'e) bak: ters çevrilmiş stop fiyatı
 // %5 sapıyorsa bu emir **bizim stop'umuz değil**
 // (yabancı/körelmiş emir). Not: `limitPx` = trigger ×
 // 1.05/0.95 olduğu için ham karşılaştırma **her zaman**
 // tutarsız döner — bu yüzden `open_orders`'a yazılan
 // ham `px` ile karşılaştırıp TERS çeviriyoruz
 // (bkz. `dedupe_sl`).
                    let known = self.sl_last_px.get(&cn).copied().unwrap_or(0.0);
                    if known > 0.0 {
                        let d = self.px_dec.get(&asset_id).copied().unwrap_or(6);
                        let trig = hyperliquid_api::round_px_for(
                            if is_buy { px / 1.05 } else { px / 0.95 },
                            d,
                        );
                        let dev = if trig > 0.0 { (trig - known).abs() / known } else { 1.0 };
                        if dev > 0.001 {
 // sessiz "stop var" SANMA — koruma illüzyonu olurdu
                            tracing::warn!(
                                "[SL-WARN] {cn} yabancı/tutarsız reduce-only emir:                                  oid={oid} seviye={:.8} beklenen={:.8} sapma={:.2}%                                  → sl_orders'a SAYILMADI (koruma bu emirden gelmiyor)",
                                trig, known, dev * 100.0
                            );
                            continue;
                        }
                    }
                    candidates.push((cn, oid));
                }
            }
        }
        Ok(candidates)
    }

 /// Aynı coin için birden fazla duran stop varsa **yalnız en yenisini** bırak.
 ///
 /// Ölçülen zarar (testnet 09:5x): GMT için oid=…61867 ve …165356, LAYER
 /// için …61871 ve …166722 aynı anda borsada duruyordu. İki reduce-only stop
 /// birden pozisyonu kapatmaya çalışıyor; borsa birini `reduceOnlyCanceled`
 /// ile iptal ediyor, `sl_orders` hangisinin ayakta kaldığını bilmiyor →
 /// bir sonraki taramada üçüncüsü ekleniyor.
 ///
 /// oid zamanla artan sayıdır (yeni emir daha büyük oid alır) → en büyük
 /// oid en yenidir. `refresh_orders`'ın döndürdüğü `(coin, oid)` listesinden
 /// yararlanır; `sl_orders`'ı da doğru şekilde doldurur.
    pub async fn dedupe_sl(&mut self, candidates: Vec<(String, i64)>) -> usize {
        let mut by_coin: HashMap<String, Vec<i64>> = HashMap::new();
        for (c, o) in candidates {
            by_coin.entry(c).or_default().push(o);
        }
        let mut cancelled = 0usize;
        for (coin, mut oids) in by_coin {
            oids.sort_unstable();
            let keep = *oids.last().unwrap();
            if oids.len() > 1 {
                for oid in oids[..oids.len() - 1].iter() {
                    match self.cancel(&coin, *oid).await {
                        Ok(()) => {
                            cancelled += 1;
                            tracing::warn!(
                                "[SL] {coin} ARTIK stop iptal edildi oid={oid} (korunan={keep})"
                            );
                        }
                        Err(e) => tracing::warn!(coin, oid, "artık stop iptal hatası: {e}"),
                    }
                    self.open_orders.remove(oid);
                }
            }
 // STOP FİYATINI REST'ten TERS HESAPLA — ÖLÇÜLEN TUZAK:
 //
 // `place_trigger(is_market = true)` gövdeyi
 // "p": trigger_px × 1.05 (alış stop, kısa kapatma)
 // "p": trigger_px × 0.95 (satış stop, uzun kapatma)
 // olarak yolluyor ve REST `openOrders` yalnız **`limitPx`**'i
 // döndürüyor. Yani `limitPx` **stop fiyatı DEĞİL**, piyasa emri
 // koruma fiyatı. Kanıt (canlı eşleşme):
 // LAYER stop kuruldu px=0.07868414 → REST limitPx=0.082614
 // 0.082614 / 1.05 = 0.078680 ✓
 // TURBO stop kuruldu px=0.00106343 → REST limitPx=0.001116
 // 0.001116 / 1.05 = 0.00106286 ✓
 // `sl_last_px` bunu tersine çevirmeden doldurulursa restart'tan
 // sonra trailing stop seviyesi %5 yanlış okunur → yanlış ratchet
 // kararı (ya da mevcut stop'un gevşetilmesi).
            let trigger_px = self
                .open_orders
                .get(&keep)
                .map(|o| {
                    let d = self.px_dec.get(&o.asset).copied().unwrap_or(6);
                    hyperliquid_api::round_px_for(
                        if o.is_buy { o.px / 1.05 } else { o.px / 0.95 },
                        d,
                    )
                })
                .unwrap_or(0.0);
            self.sl_orders.insert(coin.clone(), keep);
            if trigger_px > 0.0 {
                self.sl_last_px.insert(coin, trigger_px);
            }
        }
        cancelled
    }

 /// REST'ten pozisyonları yenile (clearinghouseState.assetPositions)
    pub async fn refresh_positions(&mut self) -> Result<()> {
        let st = self.info.clearinghouse_state(&self.user).await?;
 // KRİTİK: borsa ara sıra `null` döndürüyor (Not: 23:0x). Null'da
 // pozisyonları SİLMEK, borsadaki $400'lük açık pozisyonları "yok"
 // sayardı -> bot onları yönetmezdi. Boş/çözümsüz yanıt = hatadır,
 // eski harita korunur.
        let arr = match st.get("assetPositions").and_then(|v| v.as_array()) {
            Some(a) => a.clone(),
            None => {
                bail!("clearinghouseState çözümsüz (null) — pozisyon haritası KORUNDU");
            }
        };
        self.positions.clear();
        for ap in arr {
            let p = &ap["position"];
            let coin = p["coin"].as_str().unwrap_or("").to_string();
            if coin.is_empty() {
                continue;
            }
            self.positions.insert(coin.clone(), Position {
                szi: p["szi"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                entry_px: p["entryPx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                position_value: p["positionValue"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                unrealized_pnl: p["unrealizedPnl"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                leverage: p["leverage"]["value"].as_u64().map(|v| v as f64).unwrap_or(1.0),
                liquidation_px: p["liquidationPx"].as_str().and_then(|s| s.parse().ok()),
                coin,
            });
        }
        Ok(())
    }

 /// Kanal: "orderUpdates" -> emir durumu; "user" (userEvents) -> fill'ler.
    pub fn handle_ws_event(&mut self, channel: &str, data: &Value) {
        match channel {
            "orderUpdates" => self.apply_order_update(data),
 // SUNUCU KANALI ADI "userEvents" — "user" ile eşleştirmek
 // TÜM DOLUMLARI sessizce düşürüyordu (Not: borsada pozisyon
 // vardı, `fill(user)` sayacı 0, makine hiç pozisyona girmiyordu).
            "user" | "userEvents" | "userNonFundingLedgerUpdates" => {
                for f in data["fills"].as_array().cloned().unwrap_or_default() {
                    self.apply_fill(&f);
                }
                if data["liquidation"].is_object() {
                    tracing::warn!("LIKİDASYON eventi: {}", data["liquidation"]);
                }
            }
            _ => {}
        }
    }

 /// WS zarfı: `{"channel":"orderUpdates","data":{"orders":[ … ]}}`.
 /// Eskiden tek bir emir nesnesi bekleniyordu → `orders` dizisi gelince
 /// status/oid boş geliyor, HİÇBİR emir takip edilemiyordu (bot açık
 /// emirlerini yetim bırakıyordu).
    fn apply_order_update(&mut self, data: &Value) {
        if self.dbg_updates < 8 {
            self.dbg_updates += 1;
            let mut t = data.to_string();
            t.truncate(400);
            tracing::warn!("orderUpdate payload: {t}");
        }
        match data["orders"].as_array() {
            Some(arr) => {
                for o in arr.clone() {
                    self.apply_one_order_update(&o);
                }
            }
            None => match data.as_array() {
                Some(arr) => {
                    for o in arr.clone() {
                        self.apply_one_order_update(&o);
                    }
                }
                None => self.apply_one_order_update(data),
            },
        }
    }

    fn apply_one_order_update(&mut self, o: &Value) {
        let status = o["status"].as_str().unwrap_or("");
        let ord = &o["order"];
        let oid = ord["oid"].as_i64().unwrap_or(0);
        let coin = ord["coin"].as_str().unwrap_or("").to_string();
        if status == "open" || status == "resting" {
            let sz: f64 = ord["origSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let filled: f64 = ord["sz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let remaining = sz - filled;
            let asset_id = self.cached_asset(&coin).map(|(a, _)| a).unwrap_or(0);
            self.open_orders.entry(oid).and_modify(|e| e.sz = remaining).or_insert(OrderInfo {
                oid,
                coin: coin.clone(),
                asset: asset_id,
                is_buy: ord["side"] == "B",
                sz: remaining,
                px: ord["limitPx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                tif: Tif::Gtc,
                reduce_only: ord["reduceOnly"].as_bool().unwrap_or(false),
                ts: ord["timestamp"].as_u64().unwrap_or(0),
            });
        } else {
 // filled / canceled / rejected / marginCanceled
            self.open_orders.remove(&oid);
 // BORSA STOP'U TETİKLENDİ / İPTAL EDİLDİ: `user` kanalındaki dolum
 // 3-4 dakika gecikmeyle geliyor; bu anlık olay stop kaydından
 // düşmezse motor stop'u YENİDEN kuruyor, sonra borsa `reduceOnly`
 // gereksiz emri `reduceOnlyCanceled` ile iptal ediyor.
 // Not: TURBO stop oid=…09076 kuruldu → tetiklendi → 4 dk sonra
 // "stop iptal edildi" logu, yeni oid ile ikinci kurulum denemesi.
            if self.sl_orders.get(&coin) == Some(&oid) {
                self.sl_orders.remove(&coin);
                self.sl_armed_at.remove(&coin);
                tracing::info!(
                    "[SL] {coin} borsa stop {status} (oid={oid}) — kayıt düşürüldü"
                );
            }
            if status == "filled" {
                let sz: f64 = ord["sz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let px: f64 = ord["avgPx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let osz: f64 = ord["origSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                tracing::info!(oid, coin, sz = if sz > 0.0 { sz } else { osz }, px, "emir doldu (orderUpdates)");
                self.fill_count += 1;
            } else if status != "canceled" {
                tracing::warn!(oid, coin, status, "emir durumu");
            }
        }
    }

    fn apply_fill(&mut self, f: &Value) {
        let coin = f["coin"].as_str().unwrap_or("").to_string();
        let px: f64 = f["px"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let sz: f64 = f["sz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let oid = f["oid"].as_u64().unwrap_or(0) as i64;
        let closed: f64 = f["closedPnl"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let dir_field: &str = f["dir"].as_str().unwrap_or("");
        let mut closing_counted: std::collections::HashSet<String> = std::collections::HashSet::new();
 // TID DEDUP — aynı transaction ID iki kez işlenmesin
        let tid = f["tid"].as_str().unwrap_or("");
        if !tid.is_empty() && crate::track::tid_seen(tid) {
            tracing::warn!("[DEDUP] duplicate fill tid={tid} coin={coin} — atlandı");
            return;
        }
        self.fill_count += 1;
        self.realized_pnl += closed;
 // KAPANIŞ fill'i closedPnl=0 olsa bile (break-even) haritaya
 // yazılmalı — recon bloğunun kapanış kaydının tek güvenilir WS
 // işaretidir (0 yazmamak = break-even kapanışların kaybolması).
 // Açılış fill'lerinde closedPnl zaten 0'dır ve pozisyon açılır —
 // ayrım `dir` ile yapılır.
        if !coin.is_empty() && is_open_fill(dir_field) == false && !closing_counted.contains(&coin) {
            *self.closed_pnl.entry(coin.clone()).or_insert(0.0) += closed;
            closing_counted.insert(coin.clone());
        }
        tracing::info!(oid, coin = %coin, sz, px, closed, "fill(user)");
        if !coin.is_empty() && sz > 0.0 {
            let is_buy = f["side"].as_str().unwrap_or("B") == "B";
 // POZİSYON HARİTASINI GÜNCELLE. `refresh_positions` yalnız açılışta
 // çağrılıyordu; WS dolumu haritaya yazılmadığı için motorun
 // gördüğü `positions` açılıştan beri BOŞ kaldı → `open_now` tavanı
 // hiç tetiklenmiyor, `pz`/PnL/risk hepsi yanlış sayıyordu
 // (Not: 3 açık pozisyon varken 4. giriş açıldı).
            if self.cached_asset(&coin).is_some() {
                match self.positions.get_mut(&coin) {
                    Some(e) => {
                        if is_buy {
                            e.szi += sz;
                        } else {
                            e.szi -= sz;
                        }
                        e.position_value = px * e.szi.abs();
                    }
                    None => {
                        self.positions.insert(
                            coin.clone(),
                            Position {
                                coin: coin.clone(),
                                szi: if is_buy { sz } else { -sz },
                                entry_px: px,
                                position_value: px * sz,
                                unrealized_pnl: 0.0,
                                leverage: 1.0,
                                liquidation_px: None,
                            },
                        );
                    }
                }
 // STOP TETİKLENDİ (bu dolumun oid'i borsa stop'ununkiyle aynıysa):
 // stop artık yerinde değil, `sl_orders`'ta kalırsa döngü sonundaki
 // senkron "pozisyon hâlâ açık" görüp aynı stop'u YENİDEN
 // kurar, sonra iptal eder — Not: oid=...584 kuruldu,
 // 3 dk sonra oid=...641 tekrar kuruldu ve o da iptal edildi.
 // Borsa stop'u kendisi kapanmışsa `oid` eşleşir.
                let mut is_sl_fill = false;
                if self.sl_orders.get(&coin) == Some(&oid) {
                    is_sl_fill = true;
                    self.sl_orders.remove(&coin);
                    self.sl_armed_at.remove(&coin);
                    tracing::info!("[SL] {coin} borsa stop TETİKLENDİ (oid={oid}) — kayıt düşürüldü");
                }
 // F: TP FİLL — gerçek kapanış kaynağı. oid eşleşmesi MFE'den
 // KESİN daha güvenilirdir (MFE>0 ≠ TakeProfit).
                let is_tp_fill = if self.tp_orders.get(&coin) == Some(&oid) {
                    self.tp_orders.remove(&coin);
                    self.tp_armed_at.remove(&coin);
                    tracing::info!("[TP] {coin} TP TETİKLENDİ (oid={oid}) → ExitReason=TakeProfit");
                    true
                } else {
                    false
                };
                self.tp_fill_flags.insert(coin.clone(), (is_tp_fill, is_sl_fill));
 // pozisyon kapanırsa (stop/exit) haritadan düşsün
                if self.positions.get(&coin).map(|e| e.szi.abs() < 1e-12) == Some(true) {
                    self.positions.remove(&coin);
 // Pozisyon kapandı → TP/SL durumu temizle (borsa reduce-only
 // trigger'ları otomatik iptal eder; bize kalan kayıt
 // temizliğidir — bayat oid ile çift emir yasağı).
                    self.pending_sl.remove(&coin);
                    self.pending_tp.remove(&coin);
                    self.tp_orders.remove(&coin);
                    self.tp_armed_at.remove(&coin);
                }
            }
 // TP/SL fill bayrakları yalnız pozisyon haritası güncellenebildiyde
 // bilinir (oid eşleşmesi orada yapılır); kalan durumda false.
            let (is_tp_fill, is_sl_fill) =
                self.tp_fill_flags.remove(&coin).unwrap_or((false, false));
            self.fills_q.push(FillEvent {
                oid,
                coin: coin.clone(),
                px,
                sz,
                closed_pnl: closed,
                is_buy,
                is_tp_fill,
                is_sl_fill,
            });
 // GEÇİKMESİZ BORSA STOP: giriş dolumu geldiği anda SL kurulur.
 // (Döngü 1 saatte bir -> "sonraki döngüde" koymak pozisyonu
 // 1 saat korumasız bırakıyordu.) WS yolu senkron olduğu için
 // emir arka plan görevinde atılır; başarısızsa 3 kez denenir.
 // NOT: is_open_fill(dir) — "open" fill ise koruma kur.
 // `dir` boş geliyorsa (beklenmeyen şema) koruma KURULMAZ ve
 // uyarı loglanır (fail-safe: bayat koruma > illüzyon koruma);
 // pozisyon sonraki döngüde executor recon'undan korunur.
            let open_fill = is_open_fill(dir_field);
            if dir_field.is_empty() {
                tracing::warn!(
                    "fill'te dir alanı yok (oid={oid} coin={coin}) — giriş koruması ARMILMADI (fail-safe)"
                );
            }
            if open_fill {
                if let Some((risk, _ts)) = self.pending_sl.remove(&coin) {
 // STOP GERÇEK DOLUM FİYATINDAN. `px` = dolum fiyatı.
 // (Ölçülen hata: karar anındaki defter fiyatı + IOC kayması
 // → stop %0.9 yanlış tarafta → 1.1 sn'de tetiklendi.)
                    let mut sl = if is_buy { px - risk } else { px + risk };
 // SERT YAN KAPISI: stop dolum fiyatının **hatalı tarafına**
 // asla konulamaz (böyle bir emir zaten piyasada olduğu için
 // anında tetiklenir ve zararı büyütür — ölçülen bu). Minimum
 // mesafe %0.25; risk mesafesi zaten bu eşiğin üstündeyse
 // koşul sağlanmaz.
                    let min_d = px * 0.0025;
                    if is_buy {
                        if sl >= px || px - sl < min_d {
                            sl = px - min_d;
                        }
                    } else if sl <= px || sl - px < min_d {
                        sl = px + min_d;
                    }
                    if let Some((asset, szd)) = self.cached_asset(&coin) {
                        let ex = self.ex.clone();
                        let c = coin.clone();
                        let qty = hyperliquid_api::round_sz_up(sz, szd);
 // Tick keşfi: bu coin kaç haneye izin veriyor? (meta'da
 // YOK — Not: ME 6 haneyi reddediyor)
                        let mut dec = self.px_dec.get(&asset).copied().unwrap_or(6);
                        let tx = self.sl_tx.clone();
 // SPAWN ÖNCESİ İYİMSER KAYIT: `tokio::spawn` `&mut self`
 // yakalayamaz, yani kurulan oid'i asla `sl_orders`'a
 // yazamıyorduk — sadece loga düşüyordu. Sonuç:
 // `sl_orders` o coin için BOŞ → `sync_exchange_sl`
 // "eksik stop" görüyor → `ensure_sl` 5 dk sonra
 // AYNI POZİSYONA İKİNCİ STOP asıyordu (Not: 
 // GMT …61867 + …165356, LAYER …61871 + …166722).
 // `sl_armed_at` de yazılmadığı için grace kapısı tutmuyordu.
 // İyimser yazmak, aynı döngüdeki herhangi bir `ensure_sl`
 // çağrısını grace'e takar; gerçek oid kanal ile döner.
                        self.sl_armed_at
                            .insert(coin.clone(), hyperliquid_api::now_ms());
 // GERÇEK TP EXECUTION: TP artık borsada asılı.
 // Mesafe `pending_tp`'den (karar anında
 // `AllocationConfig::tp_from_entry` ile türetildi;
 // mesafe girişten bağımsız → doluma taşınabilir).
 // Long: TP > dolum, Short: TP < dolum (kâr tarafı).
                        if let Some((dist, _ts)) = self.pending_tp.remove(&coin) {
                            let tp = if is_buy { px + dist } else { px - dist };
                            let min_tp = px * 0.0025;
                            let tp_ok_side = if is_buy { tp - px } else { px - tp };
                            if tp > 0.0 && tp_ok_side >= min_tp {
                                let ex2 = self.ex.clone();
                                let asset2 = asset;
                                let c2 = coin.clone();
                                let qty2 = qty;
                                let dec2 = dec;
                                let is_long_tp = is_buy;
                                let tx2 = self.tp_tx.clone();
                                self.tp_armed_at.insert(coin.clone(), hyperliquid_api::now_ms());
                                tokio::spawn(async move {
                                    if !(qty2 > 0.0) || !(tp > 0.0) {
                                        return; // hayalet koruması
                                    }
                                    match place_tp_grid(&ex2, asset2, !is_long_tp, qty2, tp, dec2, &c2).await {
                                        Ok(oid) => {
                                            tracing::info!(
                                                "[TP] {c2} borsa TP kuruldu oid={oid} px={tp:.6} \
                                                 (hane={dec2}) sz={qty2} dolum={px} dist={dist:.6} \
                                                 ({}, +{:+.3}% dolumdan)",
                                                if is_long_tp { "LONG" } else { "SHORT" },
                                                (tp - px) / px * 100.0
                                            );
                                            let _ = tx2.send((c2, oid, tp));
                                        }
                                        Err(e) => {
                                            tracing::warn!("[TP] {c2} TP kurulamadı (döngüde yeniden denenir): {e}");
                                            let _ = tx2;
                                        }
                                    }
                                });
                            } else {
                                tracing::warn!(
                                    "[TP] {coin} TP mesafesi dolumun kâr tarafında değil (dist={dist:.6}) — TP kurulmadı, döngüde yeniden türetilecek"
                                );
                            }
                        }
                        tokio::spawn(async move {
                            if !(qty > 0.0) || !(sl > 0.0) {
                                return; // hayalet koruması
                            }
                            match place_sl_grid(&ex, asset, !is_buy, qty, sl, dec, &c).await {
                                Ok(oid) => {
                                    tracing::info!(
                                        "[SL] {c} borsa stop kuruldu oid={oid} px={sl} \
                                         (hane={dec}) sz={qty} dolum={px} risk={risk} \
                                         ({} taraf, {:+.3}% dolumdan)",
                                        if is_buy { "LONG" } else { "SHORT" },
                                        (sl - px) / px * 100.0
                                    );
 // oid'i sahibe geri ver (bkz. `drain_sl_results`)
                                    let _ = tx.send((c, oid, sl));
                                }
                                Err(e) => tracing::error!(
                                    "[SL] {c}: borsa stop KURULAMADI ({e}) — pozisyon yalnızca bot içinde korunuyor"
                                ),
                            }
                        });
                    } else {
                        tracing::warn!("[SL] {coin}: asset cache'te yok, stop kurulamadı");
                    }
                }
            }
        }
    }

 /// Arka planda kurulan stop'ların oid'ini `sl_orders`'a yaz.
 ///
 /// `tokio::spawn` içinden `&mut self` geçemez; bu kanal tek yazma yoludur.
 /// `run_cycle` başında çağrılır. Çağrılmazsa kurulan stop hiç kayıt
 /// altına alınmaz ve motor 5 dakika sonra aynı stop'u ikinci kez asar
 /// (ölçülen hata).
    pub fn drain_sl_results(&mut self) -> usize {
        let mut n = 0usize;
        while let Ok((coin, oid, px)) = self.sl_rx.try_recv() {
            self.sl_orders.insert(coin.clone(), oid);
            if px > 0.0 {
                self.sl_last_px.insert(coin, px);
            }
            n += 1;
        }
        if n > 0 {
            tracing::info!("[SL] {n} arka-plan stop kaydı tamamlandı (sl_orders)");
        }
        n
    }

 /// `user` kanalı dolumlarını makineye beslemek için al
    pub fn drain_fills(&mut self) -> Vec<FillEvent> {
        std::mem::take(&mut self.fills_q)
    }

    pub fn open_orders(&self) -> &HashMap<i64, OrderInfo> {
        &self.open_orders
    }
    pub fn positions(&self) -> &HashMap<String, Position> {
        &self.positions
    }
    pub fn realized_pnl(&self) -> f64 {
        self.realized_pnl
    }
 /// bekleyen KAPANIŞ fill'i olan coinler (WS closing fill'ler).
 /// Recon bloğu bunları REST 'gone' deltasıyla BİRLİŞTİRİR: `apply_fill`
 /// pozisyonu haritadan düştüğü için 'gone' delta kapanışları YUTAR —
 /// kapanış kaydının gerçek kaynağı bu haritadır.
    pub fn closed_pnl_coins(&self) -> Vec<String> {
        self.closed_pnl.keys().cloned().collect()
    }

 /// Coin'in biriken gerçekleşmiş PnL'ini oku ve SİL (pozisyon kapanınca
 /// R-çarpanı bir kez hesaplanmalı; ikinci kez sayılırsa bias olur).
    pub fn take_closed_pnl(&mut self, coin: &str) -> Option<f64> {
        self.closed_pnl.remove(coin)
    }
    pub fn fill_count(&self) -> usize {
        self.fill_count
    }
    pub fn user(&self) -> &str {
        &self.user
    }
}

/// `openOrders` REST taramasından bir satır. UNKNOWN reconciliation
/// `cloid` üzerinden eşleşir — `coin` tek başına YETMEZ (aynı coin için
/// birden fazla giriş emri olabilir).
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOrderRow {
    pub coin: String,
    pub oid: i64,
    pub cloid: Option<String>,
    pub is_buy: bool,
    pub sz: f64,
    pub px: f64,
    pub reduce_only: bool,
}

/// Batch sonucu (order/cancel index bazlı)
#[derive(Debug, Clone)]
pub struct BatchOut {
    pub oid: Option<i64>,
    pub filled_avg: Option<f64>,
    pub error: Option<String>,
 /// borsa sonucu BİLİNMİYOR (5xx/timeout). `error` string'ine
 /// bakmak yerine BU bayrak okunur — string parsing ile kontrol yapılmaz.
    pub unknown: bool,
 /// gönderilen emir cloid'i. UNKNOWN reconciliation'ı bununla
 /// eşleşir (coin tek başına YETMEZ).
    pub cloid: Option<String>,
 /// gönderilen emrin coin'i (executor asset→coin eşlemesini zaten
 /// yapıyor; çağıranın tekrar çözmesine gerek kalmaz).
    pub coin: Option<String>,
}

impl BatchOut {
    fn known(oid: Option<i64>, filled_avg: Option<f64>, error: Option<String>) -> Self {
        Self { oid, filled_avg, error, unknown: false, cloid: None, coin: None }
    }
 /// sonuç bilinmiyor — reconciliation gerekir.
    fn unresolved(cloid: Option<String>, coin: Option<String>) -> Self {
        Self { oid: None, filled_avg: None, error: None, unknown: true, cloid, coin }
    }
}

impl Executor {
 /// TOPLU YÜRÜTME: çoklu giriş emri + çoklu iptal — en fazla 2 POST
    pub async fn batch_execute(
        &mut self,
        places: &[hyperliquid_api::exchange_endpoint::WireOrder],
        cancels: &[(u32, i64)],
    ) -> anyhow::Result<Vec<BatchOut>> {
        let mut out = Vec::new();
        if !places.is_empty() {
 // Gönderim izi: hangi coin/fiyat/lot gitti (hata ayıklamanın tek
 // yolu bu — borsa "invalid price" dediğinde fiyatı nerede gördüğümüz
 // belli olmalı).
            for w in places {
                let cn = self
                    .assets
                    .iter()
                    .find(|(_, (a, _, _))| *a == w.asset)
                    .map(|(c, _)| c.clone())
                    .unwrap_or_else(|| format!("#{}", w.asset));
 // log da GERÇEKTEN giden tick'i göstersin (szDecimals değil:
 // fiyat ızgarası coin'e özel ve keşfediliyor)
                let px_w = hyperliquid_api::round_px_for(w.px, self.px_dec_for(w.asset));
                tracing::info!(
                    coin = %cn, px = px_w, px_hesap = w.px, sz = w.sz, notional = px_w * w.sz,
                    buy = w.is_buy, tif = ?w.tif, "emir gönderiliyor"
                );
            }
            let wires: Vec<hyperliquid_api::exchange_endpoint::WireOrder> = places
                .iter()
                .map(|w| {
 // fiyatı coin'in TICK'ine oturt: azami hane = keşfedilen
 // tick (varsayılan 6) ve 5 anlamlı hane sınırı.
                    hyperliquid_api::exchange_endpoint::WireOrder {
                        asset: w.asset,
                        is_buy: w.is_buy,
                        sz: w.sz,
                        px: hyperliquid_api::round_px_for(w.px, self.px_dec_for(w.asset)),
                        tif: w.tif,
                        reduce_only: w.reduce_only,
                        cloid: w.cloid.clone(),
                    }
                })
                .collect();
            let place_call = self.ex.batch_place_full(&wires).await;
            let status = match place_call {
                hyperliquid_api::exchange_endpoint::BatchPlace::Known(s) => Some(s),
 // UNKNOWN -> BILGI KAYBOLMAZ. Her place için ayrı
 // `unknown:true` BatchOut üretilir; çağıran reconciliation
 // yapar. ÖNCE: `?` ile tüm fonksiyon hata dönüyordu ve
 // `BatchOut` hiç üretilemiyordu -> engine_loop'taki recon
 // bloğu ÖLÜ koddu.
                hyperliquid_api::exchange_endpoint::BatchPlace::Unknown => None,
                hyperliquid_api::exchange_endpoint::BatchPlace::Error(e) => return Err(e.into()),
            };
            let Some(status) = status else {
 // UNKNOWN: her place için coin+cloid taşıyan satır üret
                for w in places {
                    out.push(BatchOut::unresolved(
                        w.cloid.clone(),
                        Some(self.coin_of(w.asset)),
                    ));
                }
                return Ok(out);
            };
            let status = status.raw;
            for i in 0..places.len() {
                let mut unknown = false;
                let mut oid = status.pointer(&format!("/response/data/statuses/{}/resting/oid", i))
                    .and_then(|v| v.as_i64());
                let mut avg = status.pointer(&format!("/response/data/statuses/{}/filled", i))
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.get(1))
                    .and_then(|v| v.as_f64());
                let mut err = status.pointer(&format!("/response/data/statuses/{}/error", i))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
 // TICK KEŞFİ: "invalid price" -> coin'in tick'i daha kaba.
 // (testnet: ME 6 haneyi reddediyor, AXS/BERA/MEME kabul ediyor;
 // meta'da tick bilgisi YOK -> öğrenip cache'liyoruz)
 // HAM fiyattan yola çık: kaba ızgarada fiyat 0'a çökerse
 // daha İNCE ızgaraya geç (0-lot fiyat asla gönderilmez).
                let raw_px = places[i].px;
                let mut dec = self.px_dec_for(wires[i].asset);
                let mut tries = 0;
                while err
                    .as_deref()
                    .map(|e| e.contains("invalid price"))
                    .unwrap_or(false)
                    && tries < 6
                {
                    let nxt = dec.saturating_sub(1);
                    let cand = hyperliquid_api::round_px_for(raw_px, nxt);
                    if cand <= 0.0 {
 // bu ızgara fiyatı temsil edemiyor -> daha ince ızgara
                        if dec >= 8 {
                            break;
                        }
                        dec += 1;
                        self.px_dec.insert(wires[i].asset, dec);
                        tries += 1;
                        continue;
                    }
                    dec = nxt;
                    self.px_dec.insert(wires[i].asset, dec);
                    let fixed = hyperliquid_api::exchange_endpoint::WireOrder {
                        px: cand,
                        ..wires[i].clone()
                    };
                    tracing::info!(
                        coin = ?self
                            .assets
                            .iter()
                            .find(|(_, (a, _, _))| *a == wires[i].asset)
                            .map(|(c, _)| c.clone()),
                        tick_hane = dec,
                        px_raw = raw_px,
                        px = cand,
                        "fiyat tick'i geçersiz — yeni ızgarayla yeniden deneniyor"
                    );
                    match self.ex.batch_place_full(std::slice::from_ref(&fixed)).await {
                        hyperliquid_api::exchange_endpoint::BatchPlace::Known(st2) => {
                            oid = st2.resting_oid(0);
                            avg = st2.filled(0).map(|(_, px, _)| px);
                            err = st2.error(0).map(|s| s.to_string());
                        }
 // tick keşfi sırasında UNKNOWN -> bu emir de
 // belirsiz; sessizce "başarısız" sayıp geçilmez.
                        hyperliquid_api::exchange_endpoint::BatchPlace::Unknown => {
                            unknown = true;
                            err = None;
                            break;
                        }
                        hyperliquid_api::exchange_endpoint::BatchPlace::Error(e) => {
                            err = Some(e.to_string());
                            break;
                        }
                    }
                    tries += 1;
                    if err.is_none() {
                        break;
                    }
                }
                if unknown {
                    out.push(BatchOut::unresolved(
                        places[i].cloid.clone(),
                        Some(self.coin_of(places[i].asset)),
                    ));
                } else {
                    out.push(BatchOut::known(oid, avg, err));
                }
            }
        }
        if !cancels.is_empty() {
            let status = self.ex.batch_cancel(cancels).await?;
            for i in 0..cancels.len() {
                let _ = i;
                out.push(BatchOut::known(
                    None, None,
                    status.error(i).map(|s| s.to_string()),
                ));
            }
        }
        Ok(out)
    }
}

impl Executor {
 /// coin -> (asset id, szDecimals) — cache'li, ağ çağrısı yapmaz (None: yok)
    pub fn cached_asset(&self, coin: &str) -> Option<(u32, u32)> {
        self.assets.get(coin).map(|(a, d, _)| (*a, *d))
    }

 /// asset id -> coin. Bilinmiyse `#<id>` (log'da kaybolmasin diye).
    pub fn coin_of(&self, asset: u32) -> String {
        self.assets
            .iter()
            .find(|(_, (a, _, _))| *a == asset)
            .map(|(c, _)| c.clone())
            .unwrap_or_else(|| format!("#{asset}"))
    }

}

impl Executor {
 /// açık emir akışı: (coin, oid) — watchdog usage
    pub fn open_orders_view(&self) -> Vec<(String, i64)> {
        self.open_orders
            .values()
            .map(|o| (o.coin.clone(), o.oid))
            .collect()
    }
}

#[cfg(test)]
mod tick_tests {
    use super::*;
    use hyperliquid_api::round_px_for;

 // NOT: parametre coin'in AZAMI HANE SAYISI (meta'da tick yok, borsa
 // reddedince keşfediliyor). round_px_for(px, max_decimals)
    #[test]
    fn fiyat_tick_kurali() {
 // ME (Not: 6 hane RED, 5 hane KABUL)
        assert!((round_px_for(0.073706, 5) - 0.07371).abs() < 1e-12);
 // MEME: 6 hane
        assert!((round_px_for(0.00057526, 6) - 0.000575).abs() < 1e-12);
 // SOL: 5 anlamlı hane -> 118.44
        assert!((round_px_for(118.43925, 6) - 118.44).abs() < 1e-9);
 // BTC: 84730.91234 -> 5 anlamlı hane 84731, ızgarada kalır
        assert!((round_px_for(84730.91234, 6) - 84731.0).abs() < 1e-9);
 // 5 anlamlı hane sınırı
        assert!((round_px_for(123456.789, 6) - 123460.0).abs() < 1e-9);
 // geçersiz giriş -> 0 (hayalet emir kapısı yakalar)
        assert_eq!(round_px_for(0.0, 6), 0.0);
        assert_eq!(round_px_for(f64::NAN, 6), 0.0);
    }
}

#[cfg(test)]
mod senaryo_tests {
    use super::*;

    fn ap(coin: &str, szi: &str, px: &str) -> serde_json::Value {
        serde_json::json!({"position": {
            "coin": coin, "szi": szi, "entryPx": px,
            "positionValue": "10", "unrealizedPnl": "0",
            "leverage": {"value": 5}, "liquidationPx": null
        }})
    }

 // ANALİST MADDESİ 3 — "geçerli boş assetPositions = hata" ayrımı
 // YANLIŞTI ve sistem kendini kilitlemeye meyilliydi: tüm pozisyonlar
 // gerçekten kapandığında (SL'ler borsada tetiklendi, `user` dolumu WS
 // kopması yüzünden kaçtı) her döngü "hata" sayılır → 5 hata üst üste
 // kill-switch → flatten → restart → tekrar. Testnet koşuları hep
 // pozisyonlu olduğu için görülmemişti.
    #[test]
    fn gecerli_bos_dizi_flat_hesabi_temsil_edER_flat() {
        let next = Executor::parse_positions(&[]);
        assert!(next.is_empty(), "boş dizi = hesap flat");
    }

    #[test]
    fn bos_dizi_eskiden_bos_haritada_kapanan_bildirmez() {
        let mut old: HashMap<String, Position> = HashMap::new();
        old.insert(
            "BTC".into(),
            Position { coin: "BTC".into(), szi: 1.0, entry_px: 100.0,
                position_value: 100.0, unrealized_pnl: 0.0, leverage: 5.0,
                liquidation_px: None },
        );
        let next = Executor::parse_positions(&[]);
        let gone: Vec<String> = old.keys().filter(|c| !next.contains_key(*c)).cloned().collect();
        assert_eq!(gone, vec!["BTC".to_string()]);
    }

    #[test]
    fn dolu_dizi_harita_kurar_ve_szi_parsing_dogru() {
        let next = Executor::parse_positions(&[ap("BBB", "2", "50"), ap("CCC", "-1.5", "20")]);
        assert_eq!(next.len(), 2);
        assert_eq!(next["BBB"].szi, 2.0);
        assert_eq!(next["CCC"].szi, -1.5, "kısa pozisyon negatif");
        assert_eq!(next["CCC"].entry_px, 20.0);
    }

    #[test]
    fn suregre_kapisi_tekrar_kurmayi_engeller() {
 // `sl_orders` boşken (reconnect) süre kapısı yeni stop'un kurulmasını
 // engeller. Ölçülen hata: aynı pozisyona 5 dk arayla ikinci oid.
        let armed: HashMap<String, u64> =
            HashMap::from([("BTC".to_string(), hyperliquid_api::now_ms())]);
        let age = (hyperliquid_api::now_ms() as i64 - armed["BTC"] as i64).unsigned_abs();
        assert!(age < TS_GRACE_MS, "taze kurulum grace penceresinde");
 // eski kayıt artık geçmişte
        let old = hyperliquid_api::now_ms() - TS_GRACE_MS - 1_000;
        let age_old = (hyperliquid_api::now_ms() as i64 - old as i64).unsigned_abs();
        assert!(age_old >= TS_GRACE_MS, "eski kurulum grace dışı → yeniden denenebilir");
    }
}

#[cfg(test)]
mod cloid_tests {
    use super::*;
    #[test]
    fn cloid_16_hane_hex_uretir_ve_deterministiktir() {
        let a = make_cloid("BTC", 100.0, 1.0, 3);
        let b = make_cloid("BTC", 100.0, 1.0, 3);
        assert_eq!(a, b, "aynı girdi = aynı idempotency anahtarı");
        assert_eq!(a.len(), 34, "Hyperliquid cloid = 0x + 32 hex hane (128 bit)");
        assert!(a.starts_with("0x"), "0x prefix zorunlu");
        assert!(a[2..].chars().all(|c| c.is_ascii_hexdigit()), "hex olmalı");
        let c = make_cloid("BTC", 100.0, 1.0, 4);
        assert_ne!(a, c, "farklı salt = farklı anahtar");
    }
    #[test]
    fn farkli_coin_farkli_cloid() {
        assert_ne!(make_cloid("BTC", 1.0, 1.0, 0), make_cloid("ETH", 1.0, 1.0, 0));
    }
}

#[cfg(test)]
mod stop_px_tests {
 /// REST `limitPx`'i stop fiyatı DEĞİL. `place_trigger(is_market = true)`
 /// gövdeyi `trigger × 1.05` (alış) / `trigger × 0.95` (satış) yolluyor ve
 /// REST `openOrders` yalnız `limitPx` döndürüyor. Ölçülen canlı eşleşme:
 /// LAYER stop px=0.07868414 → REST limitPx=0.082614 → /1.05 = 0.078680 ✓
 /// Bu olmadan restart sonrası stop seviyesi %5 yanlış okunur.
    #[test]
    fn rest_limit_px_ters_cevirilir_yoksa_yuzde5_kayik() {
        let trigger = 0.07868414731730063_f64;
        let is_buy = true; // kısa pozisyonu kapatmak için AL stop
        let wire = trigger * 1.05;
        assert!((wire - 0.08261835).abs() < 1e-6, "gövde fiyatı");
        let back = wire / 1.05;
        assert!(
            (back - trigger).abs() / trigger < 1e-6,
            "ters çevirme stop fiyatını verir"
        );
 // ters çevirmeyi unutursak %5'lik hata KALICI:
        assert!(
            (wire - trigger).abs() / trigger > 0.04,
            "limitPx'i doğrudan stop sanmak %5 kayık"
        );
 // satış stop tersi
        let t2 = 0.00106342669485418_f64;
        let wire2 = t2 * 0.95;
        assert!((wire2 / 0.95 - t2).abs() / t2 < 1e-9);
    }

    #[test]
    fn yon_karistirilirsa_hata_isaret_degistirir() {
 // BUY stop /1.05, SELL stop /0.95. Yanlış yön bölmesi stop'u ters
 // yöne kaydırır (long'un stop'u aşağı, short'unki yukarı gider) —
 // yani stop yerine kâr-garanti eden emir olur. En kötü hata.
        let trigger = 100.0_f64;
        let wrong = trigger * 1.05 / 0.95; // ters yön
        assert!(wrong > trigger, "yanlış yön stop'u gevşetir");
 // 1.05/0.95 = 1.10526 → %10.5 kayık
        assert!(((wrong - trigger) / trigger - 0.10526).abs() < 1e-4);
    }
}

#[cfg(test)]
mod sl_side_tests {
 /// Dolum fiyatından risk mesafesiyle stop hesabı + yan taraf sert kapısı.
 /// `apply_fill` içindeki formülün birebir kopyası (saf, ağ yok).
    fn sl_from_fill(is_buy: bool, fill_px: f64, risk: f64) -> f64 {
        let mut sl = if is_buy { fill_px - risk } else { fill_px + risk };
        let min_d = fill_px * 0.0025;
        if is_buy {
            if sl >= fill_px || fill_px - sl < min_d {
                sl = fill_px - min_d;
            }
        } else if sl <= fill_px || sl - fill_px < min_d {
            sl = fill_px + min_d;
        }
        sl
    }

 /// ÖLÇÜLEN HATA: stop, karar anındaki **defter** fiyatından hesaplanıyordu.
 /// GMT: defter entry=0.0090015 (short), gerçek dolum=0.009083 (%0.9 kayma).
 /// Mutlak fiyat kullanılırsa stop = 0.0090015 + risk, dolumun **altında**
 /// kalır → zaten piyasada → 1.1 saniyede tetiklenir (−$0.21 Not: ).
    #[test]
    fn ioc_kaymasi_stopu_yan_tarafa_tasimaz() {
        let fill = 0.009083_f64;
        let risk = 0.0000331_f64; // gerçek risk mesafesi
        let sl = sl_from_fill(false, fill, risk); // short
        assert!(
            sl > fill,
            "kısa stop dolumun ÜSTÜNDE olmalı, yoksa anında tetiklenir. sl={sl} fill={fill}"
        );
        assert!((sl - (fill + risk)).abs() < 1e-12, "mesafe korunur");
 // ESKİ (hatalı) davranışın sonucu: stop dolumun %0.9 altında
        let buggy = 0.0090015_f64 + risk;
        assert!(
            buggy < fill,
            "kanıt: eski yöntem stop'u dolumun altına koyuyordu → anında tetiklenme"
        );
    }

    #[test]
    fn kayma_ne_kadar_buyuk_olursa_olsun_stop_lehli_tarafta_kalir() {
        let fill = 100.0_f64;
        let risk = 0.3_f64; // %0.3
        for kayma_pct in [0.0_f64, 0.5, 1.0, 3.0, 9.0] {
            let fill_px = fill * (1.0 + kayma_pct / 100.0);
            let sl = sl_from_fill(false, fill_px, risk);
            assert!(sl > fill_px, "kısa: kayma %{kayma_pct} → stop yine lehli");
            let sl_l = sl_from_fill(true, fill_px, risk);
            assert!(sl_l < fill_px, "uzun: kayma %{kayma_pct} → stop yine lehli");
        }
    }

    #[test]
    fn risk_mesafesi_cok_kucukse_minimum_yuzde025_uygulanir() {
 // ATR âmı → risk ~0 → stop dolumun üstüne binerdi
        let fill = 100.0_f64;
        let sl = sl_from_fill(false, fill, 0.0);
        assert!((sl - 100.25).abs() < 1e-9, "min mesafe %0.25");
        let sl_l = sl_from_fill(true, fill, 0.0);
        assert!((sl_l - 99.75).abs() < 1e-9);
    }

 /// Ters yönlü/bozuk girdi asla "yan tarafta" stop üretemez.
    #[test]
    fn bozuk_risk_degerinde_de_stop_dogru_tarafta() {
 // negatif risk (veri bozulması), NaN, devasa risk
        for risk in [-1.0_f64, -1e9, f64::MAX, f64::MIN] {
            let sl = sl_from_fill(false, 100.0, risk);
            assert!(sl > 100.0, "short: risk={risk} → stop lehli");
            let sl_l = sl_from_fill(true, 100.0, risk);
            assert!(sl_l < 100.0, "long: risk={risk} → stop lehli");
        }
    }
}

/// UNKNOWN → gerçek cloid reconciliation kabul testleri.
#[cfg(test)]
mod recon_tests {
    use super::*;
    use crate::engine_loop::find_by_cloid;
    use crate::state_machine::{Action, Decision, PositionStateMachine, PosState, SmConfig};

    fn row(coin: &str, oid: i64, cloid: Option<&str>, reduce_only: bool) -> OpenOrderRow {
        OpenOrderRow {
            coin: coin.into(),
            oid,
            cloid: cloid.map(|s| s.to_string()),
            is_buy: true,
            sz: 1.0,
            px: 100.0,
            reduce_only,
        }
    }

    fn machine_pending() -> PositionStateMachine {
        let mut m = PositionStateMachine::new("BTC", SmConfig::default());
        let a = m.on_decision(Decision { score: 0.9 }, 99.0, 101.0, 1000);
        assert!(matches!(a, Action::Place(_)), "giriş emri bekleniyordu");
 // oid HENÜZ bilinmiyor -> Pending { oid: 0 }
        assert!(m.oid_zero_pending(), "machine oid=0 pending olmali");
        m
    }

 /// UNKNOWN sonrası cloid borsada varsa oid BAĞLANIR, pending çözülür.
    #[test]
    fn unknown_cloid_borsada_varsa_baglanir() {
        let cloid = "abcd1234";
        let rows = vec![
            row("ETH", 111, Some("ffff0000"), true),  // başka coin
            row("BTC", 222, Some("ffff1111"), false), // aynı coin, BAŞKA cloid
            row("BTC", 333, Some(cloid), false),     // aynı coin, AYNI cloid
            row("BTC", 444, None, true),             // cloid'siz stop
        ];
        let oid = find_by_cloid(&rows, "BTC", cloid).expect("cloid eslesmeli");
        assert_eq!(oid, 333, "cloid borsada varsa oid BAĞLANMALI");

        let mut m = machine_pending();
        assert!(m.oid_zero_pending());
        m.link_pending_oid(oid);
        assert!(
            matches!(m.state, PosState::Pending { oid: 333, .. }),
            "link sonrasi oid 333 olmali, got {:?}",
            m.state
        );
        assert!(!m.oid_zero_pending(), "pending cozulmus olmali");
 // log formatı: "[RECON] cloid=… coin=… oid=… ile bağlandı"
        let log = format!("[RECON] cloid={cloid} coin=BTC oid={oid} ile bağlandı");
        assert!(log.contains(&format!("cloid={cloid}")));
        assert!(log.contains("coin=BTC"));
        assert!(log.contains("oid=333"));
        assert!(log.ends_with("ile bağlandı"));
    }

 /// UNKNOWN sonrası cloid borsada YOKSA pending -> Flat revert.
    #[test]
    fn unknown_cloid_yoksa_flat_revert() {
        let cloid = "deadbeef";
 // Aynı coin'de duran BAŞKA cloid'li emir var — YANLIŞ eşleşme olmamalı
        let rows = vec![
            row("BTC", 222, Some("ffff1111"), false),
            row("BTC", 444, None, true),
        ];
        assert!(
            find_by_cloid(&rows, "BTC", cloid).is_none(),
            "cloid yoksa ESLESME olmamali (coin tek basina yeterli DEGIL)"
        );

        let mut m = machine_pending();
        assert!(m.oid_zero_pending());
        if m.oid_zero_pending() {
            m.state = PosState::Flat;
        }
        assert!(
            matches!(m.state, PosState::Flat),
            "cloid yoksa pending Flat revert etmeli, got {:?}",
            m.state
        );
        let log = format!("[RECON] cloid={cloid} coin=BTC borsada YOK -> machine Flat revert");
        assert!(log.contains("borsada YOK"));
        assert!(log.ends_with("machine Flat revert"));
    }

 /// Coin ESLESSE bile cloid farklıysa bağlanmaz — çakışma koruması.
    #[test]
    fn ayni_coin_farkli_cloid_eslesmez() {
        let rows = vec![row("BTC", 555, Some("bir"), false), row("BTC", 666, Some("iki"), false)];
        assert_eq!(find_by_cloid(&rows, "BTC", "bir"), Some(555));
        assert_eq!(find_by_cloid(&rows, "BTC", "iki"), Some(666));
        assert_eq!(find_by_cloid(&rows, "BTC", "üç"), None);
 // farklı coin, aynı cloid -> eşleşmez
        assert_eq!(find_by_cloid(&rows, "ETH", "bir"), None);
    }

 /// ENTRY emri (reduce_only=false) refresh_orders listesinde YOKTUR.
 /// Reconciliation onu `refresh_open_orders_all` ile bulmalı.
    #[test]
    fn refresh_orders_entry_emrini_gormez_hepsi_gorur() {
        let entry = row("BTC", 777, Some("c777"), false);
        let stop = row("BTC", 888, Some("c888"), true);
 // parse_open_orders her ikisini de döner
        assert_eq!(entry.cloid.as_deref(), Some("c777"));
        assert!(!entry.reduce_only, "giris emri reduce_only=false");
        assert!(stop.reduce_only);
    }
}
