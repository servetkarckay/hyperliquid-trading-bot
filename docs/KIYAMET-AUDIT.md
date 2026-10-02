# KIYAMET AUDIT — 20 LIKİDASYON SENARYOSU

> Perspektif: Wall Street quant + paranoid Rust mimarı. Her senaryo için önce
> **KOD GERÇEĞİ** (okunabilir ve doğrulanabilir), sonra risk sınıfı, sonra çözüm.
>
> **RİSK SINIFLARI:** 🟥 PARA KAYBETTİRİR · 🟧 CHAT/STUCK · 🟨 KAYIP VERİ · 🟩 KAPALI
> **DURUM:** ✅ ÇÖZÜLDÜ (kodda + testte) · 🟡 KISMİ · ⬜ YAPILMADI

Bu metin **2026-10-02'de sıfırdan yazıldı.** Önceki sürümde satırlara Rusça/
Çince/İngilizce karakter dizileri karışmıştı ("SL WOMAN dolumu", "charities",
"PÜZXA", "kameri çaketsiz", "İoly nadeno"); o sürüm karar vermeye güvenilir
değildi ve **silindi**. Aşağıdaki her "KOD GERÇEĞİ" satırı bu depodan
doğrulanmıştır.

---

## 1. RUST SİSTEM MÜHENDİSLİĞİ

### S1. Deadlock / Lock Contention
**KOD GERÇEĞİ:** `Arc<RwLock>` **kullanılmaz**. Engine state tek task'a aittir
(`bot ana döngüsü`); WS olayları `ingest_oe` ile aynı task'ta işlenir. Klasik
kilit açlığı yapısal olarak imkânsız.
**RİSK:** 🟧 Tek konsümitör darboğazu. `run_cycle` içindeki REST çağrıları
(`rest_snapshot`, `clearinghouseState`) bloklanırsa WS event'leri kuyruklanır.
**ÇÖZÜM:** ⬜ `run_cycle` ağ çağrılarını `tokio::time::timeout` ile sarmala;
ölçüm: WS kuyruk derinliği + en uzun `run_cycle` süresi loglanmalı.
**NOT:** `ENGINE_CYCLE_SECS` (varsayılan 5 sn) döngü periyodunu belirler;
`ENGINE_INTERVAL` **mum** interval'idır ve döngüyü kontrol etmez.

### S2. Panic & Unwinding
**KOD GERÇEĞİ:** `run_cycle` stratejileri **inline** çağırır. `catch_unwind`
yok, `spawn` izolasyonu yok. `panic = "unwind"` zaten Cargo'nun varsayılanıdır
(ayrıca sabitlemek gerekmez; `abort` yalnızca daha hızlı öldürür).
**RİSK:** 🟥 Tek stratejinin paniği tüm botu düşürür → açık pozisyonlar
sahipsiz kalır. **AZALTICI (yeni):** her açık pozisyon artık borsada gerçek
reduce-only trigger SL ile korunuyor (bkz. S8) → bot ölse bile pozisyon kapanır.
**ÇÖZÜM:** ⬜ Strateji başına `tokio::spawn` + `JoinHandle` (JoinError::is_panic
→ `Signal::Skip`). Supervisor deseni: görev çökerse yalnızca o görev re-spawn
edilir.

### S3. Memory Leak
**KOD GERÇEĞİ:** Büyüyen sınırlar:
- `Aggregator.history` — coin+iv başına **zaman penceresi** (varsayılan 500;
  canlı botta eğitimle eşit açılır: 1h→4400, 4h→1100, 1d→500;
  `ENGINE_HIST_BARS × bar_interval`). `backfill` de aynı pencereye kırpılır
  (daha önce kırpılmıyordu → sonsuz büyüme).
- `MarketCtx` oi/funding serileri, tracker rolling-100 — cap'li.
- `entry_attr` — her döngü başında `retain(now − ts < 24h)` ile **temizleniyor** ✅
- `Executor.px_dec` (tick keşfi) — coin sayısıyla sınırlı, sorun değil.
- `Executor.sl_orders` — açık stop sayısıyla sınırlı, kapanışta silinir ✅
**AÇIK:** `open_orders` içinde reddedilmiş-emir kalıntısı ve `fills_q` için
sınır yok (döngü başında `drain_fills` tüketir → pratikte sınırlı).
**ÇÖZÜM:** ⬜ Watchdog sweep: `open_orders` yaşı > 5 dk olan oid'leri düşür.

### S4. Channel Backpressure
**KOD GERÇEĞİ:** `ingestor::start` bounded(8192) mpsc; yazar `tx.send().await`
bekler → kanal doluysa WS frame okuma durur → sunucu ping/pong'u erteler ve
bağlantıyı KAPATIR.
**RİSK:** 🟧 Sessiz WS kesintisi.
**ÇÖZÜM:** ⬜ `try_send()` + drop-oldest + "son durum" cache'i (lossy mum
yapısı). Ölçüm: kuyruk doluluk oranı + drop sayacı loglanmalı.

---

## 2. BORSA ALTYAPISI

### S5. Stuck Order (Orphan emir) — ✅ ÇÖZÜLDÜ ve TEST EDİLDİ
**KOD GERÇEĞİ:** Üç katman:
1. POST dönüşü gelirse `link_pending_oid` bağlar.
2. Yanıt **hiç gelmezse** (timeout) → oid bilinmez → borsada emir var, bizde yok.
3. **Açılış yetim temizliği:** `Engine::new` içinde `RECON` — borsadaki açık
   emirler taranır, **pozisyonu olmayan** her emir iptal edilir.
**CANLI TEST (2026-10-02, testnet):** `probe_px --rest BTC` ile imzasız
olmayan, defterde **asılı** bir GTC emir bırakıldı (BTC alış 0.00024 @ 80686,
oid=61606399231). `openOrders` ile doğrulandı. Bot yeniden başlatıldı →
```
[RECON] açılışta yetim emir iptal: BTC/61606399231
[RECON] 1 yetim emir iptal edildi
```
**SONUÇ: 🟩** Yetim emir açılışta deterministik olarak iptal ediliyor.
**EKSİK:** Aralıklı (periyodik) recon yok — yalnızca açılışta. Watchdog
`reconcile` (her 30 sn) ⬜ yapılacak. Yanıt-kayıp oid'i `refresh_orders` ile
geri bağlama (bulma) ⬜.

### S6. Phantom Fills (hayalet dolum / çift sayım)
**KOD GERÇEĞİ:** `orderUpdates` emir DURUMU için, `user` kanalı DOLUM için
gerçek kaynak (IOC tam dolumunda `orderUpdates` `sz=0.0` geliyordu → yanlış
boyut kaydediliyordu; ölçüldü ve düzeltildi). **Ama `tid` bazlı dedup YOK**
(kodda `HashSet<tid>` bulunamadı) → aynı dolum iki kanaldan gelirse tracker
iki kez sayabilir.
**RİSK:** 🟧 Win/loss istatistiği çift sayım → yanlış terfi kararı.
**ÇÖZÜM:** ⬜ `Executor.dedup: HashSet<i64>` (tid) + **rolling pencere** (S3'ün
kendisi bu kümeyi şişirir — sınırsız HashSet olmamalı).

### S7. API Rate Limiting + Backoff
**KOD GERÇEĞİ:** `api/src/signing.rs::post` içinde **429/5xx için exponential
backoff var** ✅ (eski metin "backoff yok" diyordu — YANLIŞTI). Döngü başına
tek place POST + tek cancel batch → 429 riski düşük.
**RİSK:** 🟥 Retry'in **kendi getirdiği** çift emir riski: 5xx'te istek
borsada işlenmiş olabilir, yanıt kaybolmuştur → retry aynı emri ikinci kez
yollar → pozisyon ikiye katlanır. S5'in ("borsada emir var, bizde yok")
çözdüğü senaryonun ikizi.
**ÇÖZÜM:** ✅ **`cloid` eklendi** (bulgu #29): deterministik `0x`+32 hex, emir
nesnesinin içinde `"c"`. Borsa aynı cloid'i ikinci kez reddeder → idempotency.
Canlıda doğrulandı: yanlış format (16 hane) → HTTP 422, doğru format → emir
geçti (`[FLAT] hesap temiz`).
**KALAN:** 🟧 Backoff sırasında emir fiyatı eskiyor (5xx sonrası 0.5–8 sn
arası). Yeniden gönderilecek emirler güncel mark/bid-ask ile tazelensin;
`userRateLimit` ile tüketim ölçülmeli.

### S8. Borsa Bakımı / State Drift — ✅ BÜYÜK ÖLÇÜDE ÇÖZÜLDÜ
**ESKİ AÇIK:** "borsada gerçek trigger SL yok; bot uyur/kill-switch'e girerse
pozisyonlar sahipsiz kalır" — bu senaryonun **en ölümcül kombinasyonuydu**
(`place_trigger` API'si zaten vardı, kullanılmıyordu).
**YAPILAN (2026-10-02):**
1. **Gecikmesiz borsa stop.** `Executor::ensure_sl` / `apply_fill`: giriş
   dolumu `user` kanalından gelir **gelmez**, hedef SL fiyatından
   (`entry_attr.risk_px`) reduce-only trigger SL atılır (3 deneme, arka plan
   görevi). Döngü 5 dk'da bir olduğu için "sonraki döngüde koy" yaklaşımı
   pozisyonu saatlerce korumasız bırakıyordu.
2. **Her döngü sonunda senkron.** `Engine::sync_exchange_sl()`: açık her
   pozisyonun borsada SL'si var mı diye bakar, yoksa kurar (restart sonrası
   devralınan pozisyonlar dahil; kayıt yoksa %2 kurtarma stop'u). Kapalı
   pozisyonun stop'u iptal edilir.
3. **Kill-switch artık düzleştiriyor.** 5 hata üst üste → önce
   `flatten_positions()`, sonra duraklat. Önceden pozisyonlar sahipsiz kalıyordu.
4. **`ENGINE_FLATTEN=1`** açılışta tüm pozisyonları kapatır (genişleyen kapak:
   %1 → %3 → %6 → %9; testnet defteri ince olduğu için tek kapak yetmiyordu).
**KOD GERÇEĞİ (risk kapısı):** `pre_trade()` **yalnızca GİRİŞ** emirlerinde
çağrılır (`engine_loop` Place kolu). **Çıkışlar `pre_trade`'dan geçmez** →
drawdown halt (%20) veya equity tabanı ($50) tetiklense bile **çıkış emirleri
serbesttir**. ✅ Bu, "halt'ın kendisi en kötü senaryoyu tetikler" tuzağını
kapatır.
**KALAN:** ⬜ `StateMaintenance` (REST 3 kez üst üste hata → exits_only mod)
yok. WS kopmasında bot stale mumlarla çalışmaya devam ediyor.

---

## 3. PİYASA MİKRO-YAPI

### S9. Spoofing / Layering
**KOD GERÇEĞİ:** LOB imbalance üst 5 seviyeden geliyor; spoof bunu %90+ sahte
yapabilir. **`microprice` kodda YOK** (yalnızca bu dokümanda geçiyordu).
**RİSK:** 🟥 Sahte dengesizlik → sahte sinyal.
**FORMÜL DÜZELTMESİ:** Önceki metin yanlıştı:
`(bid·askSize + ask·bidSize)/(bid+ask)` → payda **fiyatların toplamı**, anlamsız.
Doğrusu: **`(bid·askSz + ask·bidSz)/(bidSz + askSz)`** — ağırlıklar hacimle
normalize edilir, sonuç fiyat aralığında kalır.
**ÇÖZÜM:** ⬜ (a) OFI: seviye DELTALARININ (Δbid/Δask) EMA'sı — sahte büyük
emirlerde imbalance sabit kalırken OFI değişmez; (b) spoof skoru: seviye
hacminin kısa-uzun ortalamaya oranı + iptal hızı; (c) microprice-imbalance
tabakası.

### S10. Flash Crash / Sıfır Likidite — 🔄 GİRİŞ STRATEJİSİ DEĞİŞTİ
**KOD GERÇEĞİ:** Girişler artık **ALO (post-only) DEĞİL**, `Tif::Ioc` +
mark tabanlı **%0.5 kayma kapağı**. Nedeni (ölçüldü): ALO emir long'da ask'a /
short'ta bid'e yazılıyordu ve spread'i kesiyordu → "Post only order would have
immediately matched" (11 emrin 4'ü hiç girmiyordu). IOC "kendi kendine slippage
koruması" sağlamaz → **S10'un eski koruması artık YOK**, yerine mark kapağı var.
**ESKİ METİNİN HATASI:** "IOC'yi reddet" önermesi **çıkışta** kritik yanlıştır:
pozisyon çökerken çıkışı kilitlemek boşlukta bırakır.
**DOĞRU ÇÖZÜM:** (a) Çıkışta **agresif limit**: IOC'yi reddetme, ters yönde
mark −%3 kapağıyla IOC at (defter inceyse fill eder, reddedilmez); (b)
`vwap_fill` ile ön-dolum maliyetini hesapla, mark'tan >%1 sapıyorsa parçala;
(c) 1 barlık getiri >%5 → emir dondurma (freeze).
**AÇIK ÇELİŞKİ:** IOC taker ücreti (maker değil) → PnL modeli taker maliyetiyle
güncellenmeli. Ayrıca `market_making.rs` Avellaneda-Stoikov **maker** olmak
zorunda; global "giriş IOC" kararı MM ile çelişir → **giriş TIF'i strateji
bazlı olmalı** (⬜).

### S11. Funding Trap
**KOD GERÇEĞİ:** Funding serisi EMA ile yumuşatılmıyor; ani spike (ör. 8 saatte
+%50) eşiği aşar → short açılır → funding normalleşir + fiyat baskısı → zarar.
**RİSK:** 🟥 MIMARİ COOLDOWN YOK.
**ÇÖZÜM:** ⬜ `funding_ema(3 okuma)`; anlık değer ile EMA farkı > 5σ → SKIP
("funding anomalisi") + 15 dk cooldown.

### S12. Oracle / Mark Divergence
**KOD GERÇEĞİ:** `ctx` oracle ve mark'ı **ayrı** tutuyor; ATR mark mumlarından
geliyor → oracle-mark açılması ATR'de görünmüyor.
**RİSK:** 🟧 Kötü oracle fiyatı yanlış SL/TP üretir.
**ÇÖZÜM:** ⬜ divergence guard: `(mark−oracle)/mark > %0.5` → yeni girişleri
skiple, açık pozisyonlarda risk marjını ×3 yap.

---

## 4. RİSK & PORTFÖY KAOSU

### S13. Bakiye Kuyruğu / SL sonrası hızlı re-entry
**KOD GERÇEĞİ:** Equity döngü başında REST'ten alınır (`clearinghouseState`
`accountValue`); `null` yanıtta **eski equity korunur** ✅. Aynı döngüde hem SL
hem yeni sinyal olursa equity **STALE** olabilir → bütçe yanlış hesaplanır.
**RİSK:** 🟧
**ÇÖZÜM:** ⬜ `pre_trade`'a serbest teminat (`margin_free_usd`) parametresi;
SL kapanışı sonrası adayları `QueueEntry{ts}` ile bir döngü beklemesi.

### S14. Correlation Breakdown (Black Swan)
**KOD GERÇEĞİ:** Korelasyon filtresi (>%70 eleme) + toplam maruziyet cap var.
Ama kripto korelasyonu panikte 1.0'a kaçar ve **sizing conviction-temelli**,
kovaryans-temelli değil.
**RİSK:** 🟥 Kaskad likidasyon.
**ÇÖZÜM:** ⬜ (a) Kriz modu: evren ortalaması |getiri| > %5 ise
`max_correlation` 0.35'e iner; (b) `min_variance_weights` ile portföy boyutu;
(c) VaR(n95) > equity %10 → yeni giriş yasak; (d) evren geneli −%10/5 dk →
market sweep ile tüm pozisyonları kapat.

### S15. Stale Data & ATR=0 / Sıfıra Bölme
**KOD GERÇEĞİ:** WS durursa son fiyat mumu değişmez → RSI/ATR sabit kalır.
ATR=0 → SL=mid tehlikesi **kapalı**: tahsis motorunda `min_dist = entry×%0.25`
guard var ✅. `px <= 0` kontrolleri giriş **ve** çıkış yolunda var ✅.
**RİSK:** 🟨
**ÇÖZÜM:** ⬜ `Aggregator::is_stale(coin, iv, max_age)` → her döngüde
strateji girdisi `SKIP`.

### S16. Partial Fill & Niyet Karmaşası
**KOD GERÇEĞİ:** Kısmi dolumda `InPosition.size` **gerçek dolumdan** gelir
(makine 10 birim istedi, 3 doldu → 3 yönetilir) ✅. Doğru davranış.
**RİSK:** 🟧 Bütçe defteri 10 birim ayırdı, pozisyon defteri 3 → iki defter
uyumsuz.
**ÇÖZÜM:** ⬜ Kalan bütçeyi iade et (`AllocationLedger`) + `machine.size` ile
borsa pozisyonu %2'den fazla ayrışırsa borsa authoritative kabul edilip
senkronlanmalı.

---

## 5. AĞ & ZAMAN

### S17. NTP Çöküşü
**KOD GERÇEĞİ:** NTP median + 2 gönderici; nonce penceresi 48 saat → 500 ms
skew zararsız. 30 dk'da resync ✅.
**RİSK:** 🟩
**ÇÖZÜM:** ⬜ |offset| > 5 sn → WARN + emirleri dondur (imza geçerliliği için).

### S18. WS/REST Clock Skew
**KOD GERÇEĞİ:** Emir fiyatları mark tabanlı (oracle, tek kaynak); pozisyonlar
REST'ten. Makine kararı WS fiyatını kullanır.
**RİSK:** 🟩 Düşük.

---

## 6. BACKTEST PARADOKSU

### S19. Look-ahead Bias — 🟥 **AÇIK VE ETKİSİ BÜYÜK**
**KOD GERÇEĞİ:** Backtest bar `i` kapanışında sinyal üretiyor ve **aynı barın
kapanışında fill** yapıyor → kapanışı bilmek = geleceği görmek → kâr şişer.
**ESKİ METİNİN HATASI:** "backtest gerek yok, bekler" deniyordu. Bu **mantık
hatası**: ensemble ağırlıkları (`config/ensemble_weights.json`) **zaten bu
biased motorla eğitildi**. Yani prod ağırlıkları biased motorun çocuğu.
Düzeltme 1-2 satır değil, **ağırlıkların düzeltilmiş motorla yeniden
üretilmesini** gerektirir.
**ETKİ BOYUTU:** ~53 oy → yalnız 2 strateji eğitimi geçti; biased motor
kazananları şişirdiği için canlıda hepsi ölü olabilir.
**ÇÖZÜM (sıralı):** (1) fill `candles[i+1].open` fiyatına + slippage; (2) aynı
motorla ağırlıkları **yeniden eğit**; (3) yalnız ondan sonra canlıya güven.
⏳ Bekliyor.

**KARAR PAKETİ (bu turun analisti "tek karara bağlanmalı" dedi — ölçüldü):**

| Soru | Ölçüm | Sonuç |
|---|---|---|
| Fee modeli maker mı taker mi? | `backtest.rs`: `taker_fee_bps = 4.5`, `slippage_bps = 5.0` | ✅ **zaten taker**. Analistin "maker ücretiyle yazılmış olabilir" varsayımı **yanlış**; şişme kaynağı fee değil S19 |
| Motorda kaç strateji geçebilir? | 2/18 (biased motorda bile) | Gerçekçi beklenti: düzeltilmiş motorda **0–2**. 0 ise keşif şeridi tek yol |
| Keşif hızı | bütçe $300 → havuz %5 = **$15** → `min(N, ⌊$15/$10⌋)` = **1 kişi** | 13 keşifçi × n≥30 ≈ **aylar–yıllar**. Keşif bütçesi büyütülmezse bu şerit pratikte ölü |

Yani pakette **üçü ayrı değil**: S19 düzeltmesi → yeniden eğitim → geçen
strateji sayısı → keşif havuzu büyüklüğü. Önce S19 düzeltilmeli, sonra
eğitim çalıştırılmalı, sonra bütçe kararı verilmeli. Bu turda motor
**doğruluğu** kanıtlandı; kârlılık kanıtlanmadı ve S19 kapanmadan
kârlılık iddiası mümkün değil.

### S20. Kelly Dinamizmi — 🟡 KISMİ
**KOD GERÇEĞİ:** Tracker rolling-100 gerçek dolumlardan besleniyor ✅;
`dyn_weights()` her döngü başında ✅; ağırlık JSON dump 5 dk'da ✅. Keşif
terfi ölçütleri: n≥30, kazanç≥0.55, PF≥1.30; düşüşte geri alma var ✅.
**AÇIK:** (1) `hl_strategy_performance`'a canlı upsert yok; (2) **restart'ta
`entry_attr` + rolling tracker RAM'de** → terfi/düşüş kararları boş pencereyle
yeniden başlıyor (ağırlıklar JSON'da yaşıyor, istatistikler yaşamıyor);
(3) R-çarpımı kaydı canlı dolum yolunda besleniyor ama restart'ta kayboluyor.
**ÇÖZÜM:** ⬜ `track` durumunu diske yaz (5 dk dump zaten var → aynı dosyaya).

---

## CANLI KOŞU SONUÇLARI

| Koşu | Senaryo | Panik | Davranış | Sonuç |
|---|---|---|---|---|
| bot4 | tick boyutu kaçağı | 0 | yakalandı → düzeltme | 🟩 |
| alloc2 | tahsis + korelasyon | 0 | BTC/ZEC elendi | 🟩 |
| chaos | sahte coin + stale + stale REST | 0 | graceful SKIP | 🟩 |
| doomsday | gerçek fiyatla yeşil alış | 0 | SL/TP + tahsis | 🟩 |
| **S5 orphan (testnet)** | asılı GTC emir bırakıldı | 0 | `[RECON] 1 yetim emir iptal edildi` | **🟩** |
| **flatten + doğrulama** | gerçek kapatma emirleri | 0 | `[FLAT] hesap temiz` | **🟩** |
| **cloid doğrulaması** | 16 haneli cloid → 422, 32 haneli → emir geçti | 0 | `[FLAT] hesap temiz` | **🟩** |

> "testnet tam koşu — 30+ pozisyon" satırı buradan **kaldırıldı**. O koşu
> bug #12'nin koşusuydu (`TOP_N=3` iken 30 pozisyon birikmişti); ✅ işareti
> hatayı kanıtlıyordu, düzeltmeyi değil. **0 panik hiçbir invariant'ı kanıtlamaz** —
> bu yüzden artık testler invariant assertion içeriyor:
> `positions.len() ≤ TOP_N`, `tahsis ≤ max_position_usd`, tahsis/risk tavanı
> eşitliği, geçerli boş `assetPositions` = flat.

---

## S5 SENARYOSU TEST EDİLDİ (yetim emir)

`probe_px --rest BTC` ile bırakılan GTC emir (oid=61606399231) `openOrders`
üzerinden doğrulandı; bot yeniden başlatıldığında `[RECON] açılışta yetim emir
iptal: BTC/61606399231` → `[RECON] 1 yetim emir iptal edildi`. Yani S5'in çözümü
kâğıtta değil, **ölçülmüş** çalışıyor.

## BORSA STOP'U UÇTAN UCA DOĞRULANDI

| Adım | Kanıt |
|---|---|
| Giriş dolumu | `fill(user) oid=61631249310 coin=ALGO px=0.1268` |
| Stop kuruldu (aynı saniyeler içinde) | `[SL] ALGO borsa stop kuruldu oid=61631251772 px=0.12803` |
| Stop **gerçekten tetiklendi** | `fill(user) coin=ALGO px=0.12977 closed=-0.35343` |

Bot çalışırken öldürülse bile stop borsada durduğu için pozisyon kayıpsız kapanır.
Bu, "kill-switch + SL yok = yetim pozisyon" açığının çözümüdür.

---

## CANLI TESTNET BULGULARI (yalnız gerçek emir akışında görünür)

| # | Hata | Etki | Durum |
|---|---|---|---|
| 1 | `orderUpdates` payload **dizi** (`[{order,status}]`), kod tek nesne bekliyordu | hiçbir emir izlenemiyordu | ✅ |
| 2 | ALO giriş spread'i kesiyordu | 11 emrin 4'ü hiç girmiyordu | ✅ IOC + %0.5 kapağa |
| 3 | IOC tam dolumunda `orderUpdates` `sz=0` | makine 0 dolum sanıyordu | ✅ `user` kanalı |
| 4 | Red durum tipleri dar liste | makine `Pending`'de kilitli | ✅ `*Rejected` + `marginCanceled` |
| 5 | Ters-sinyal çıkışında `sz: 0.0` | 0-lot reduce-only (hayalet) | ✅ gerçek boyut |
| 6 | `round_5sig` yazılmış ama **hiç çağrılmamıştı** | fiyat tick'e oturmuyor | ✅ |
| 7 | Fiyat tick'i coin'e özel, meta'da yok | her denemede `invalid price` | ✅ keşif + retry |
| 8 | Kaldıraç clamp'siz | "Invalid leverage value" | ✅ |
| 9 | `clearinghouseState` ara sıra **`null`** | $400'lük pozisyon "yok" sayılıyordu | ✅ null = hata |
| 10 | Keşif havuzu ikiye bölününce $7.5 | keşif şeridi ölü | ✅ kişi başı $10 taban |
| 11 | **`history_len` sabit 500** | eğitim 4400 bar, canlı 500 → **ufuk %87'si kör**; ilk mumda `pop_front` 4400→500 | ✅ zaman penceresi |
| 12 | `cap_positions` yalnızca **döngü seçimini** sayıyordu, açık pozisyonları saymıyordu | `TOP_N=3` iken **30 pozisyon** birikti | ✅ açık pozisyon tavanı + aynı coin tekrar giriş yasak |
| 13 | Keşif tabanı ile %5 tavanı **çelişiyordu** (13 keşifçi → $130 > hesap) | sessiz ölüm veya tavan aşımı | ✅ `n = min(N, ⌊havuz/$10⌋)` |
| 14 | Kill-switch pozisyonları sahipsiz bırakıyordu | bot durunca açık pozisyonlar korumasız | ✅ düzleştirme + borsa stop |
| 15 | Borsada hiç SL yoktu (`place_trigger` kullanılmıyordu) | en ölümcül açık | ✅ gecikmesiz trigger SL |
| 16 | `ENGINE_INTERVAL` döngü periyodunu kontrol etmiyordu (5 sn sabit) | 1h stratejide saatte 720 değerlendirme | ✅ `ENGINE_CYCLE_SECS` |
| 17 | Fiyat tick'i `place_trigger`'da yuvarlanmıyordu | SL reddi | ✅ |
| 18 | Borsa dolumları `channel:"userEvents"` ile geliyor, kod `"user"` ile eşleştiriyordu | **tüm dolumlar sessizce düşüyordu** | ✅ `user\|userEvents\|userNonFundingLedgerUpdates` |
| 19 | `pending_sl` "pozisyon yok → sil" kuralı, dolum `run_cycle` dönmeden gelmediği için stop'u siliyordu | BORSA-SL:0, hiç stop kurulmuyordu | ✅ `(fiyat, ts_ms)` + 10 dk yaşlanma |
| 20 | `flatten_all` uç fiyat keşfi olmayan `place_limit` kullanıyordu | ME 4 kapama fiyatında da `invalid price` → hiç kapanmıyor | ✅ `batch_execute` (%1→%3→%6→%9) |
| 21 | **Tahsis tavanı yoktu** — bütçe tek güçlü adaya tamamen gidiyordu (bütçe $300 → `$300` tahsis) | risk kapısı `299.99 > 50.00` ile reddediyor, **sinyal boşa** | ✅ `max_position_usd` tahsiste de uygulanıyor (`$157 → $50`) |
| 22 | Kill-switch yalnız `break 'restart` ile duruyordu | bot durunca açık pozisyonlar borsada korumasız | ✅ önce `flatten_positions()` |
| 23 | **`Executor::apply_fill` pozisyon haritasını hiç güncellemiyordu**; `refresh_positions` yalnız açılışta çağrılıyordu | motorun gördüğü `positions` **açılıştan beri boş** → `open_now` tavanı hiç tetiklenmiyor (ölçüldü: 3 açık pozisyon varken 4. giriş açıldı), `pz`/PnL/toplam notional yanlış | ✅ WS dolumu haritayı yazar + her döngüde `sync_positions_from` (aynı REST yanıtından bedava) |
| 24 | `already_held` kontrolü `pmap`'e bakıyordu; `pmap` state machine'lerden kuruluyor ve `szi`'si `Default`=0 | "zaten tutuyorum" kontrolü **her zaman false** — ZK açıkken aynı coine ikinci kez, üstelik ters yönde giriş; pozisyon ikiye katlandı | ✅ `executor.positions()` (borsanın verisi) |
| 25 | Borsa stop'u tetiklenince `sl_orders` kaydı düşmüyordu | stop iki kez kurulup iptal ediliyordu (ölçüldü: oid=…584 → …641 → iptal) | ✅ oid eşleşmesi + `sync_positions_from` kapanan coin'i de temizliyor |
| 26 | Öğrenilen tick'ler (`px_dec`) süreç sonunda yok oluyordu | her restart'ta ME gibi coinlerde "invalid price" faturası tekrar | ✅ `config/tick_cache.json` (`[TICK] 1 coin diskten geri yüklendi`) |
| 27 | **WS reconnect `sl_orders`'ı siliyordu** (testnet 502/reset sık) | aynı stop ikinci kez kuruluyor; borsa birini `reduceOnlyCanceled` ile iptal ediyor | ✅ `refresh_orders` stop'ları geri yükler + `sync_exchange_sl` eksik sayıda tarar |
| 28 | S5 yetim-emir watchdog'u stop emirlerini **yetim** sayıyordu (`machines`'ta yoklar) | stop iptal edilirse giriş pozisyonu borsada korumasız | ✅ `sl_orders` muafiyeti |
| 29 | **`signing::post` 429/5xx retry'i emri iki kez yolluyordu**, `cloid` yoktu | istek işlenmiş ama yanıt kaybolmuşsa pozisyon **ikiye katlanır** — S5'in ikizi, retry yüzünden bilinçli üretiliyor | ✅ `cloid` (`0x` + 32 hex, deterministik). **Yanlış format canlıda kanıtlandı**: 16 hane → HTTP 422, tüm emirler reddedildi |
| 30 | Stop `sl_orders`'a bakarak idempotentti; WS "open" bildirimi geç geliyor | aynı stop **5 dakika sonra ikinci oid** ile kuruluyordu (ölçüldü: ALT/BIGTIME/TURBO 08:48 → 08:53) | ✅ `sl_armed_at` + `TS_GRACE_MS` = 10 dk |
| 31 | `entry_attr` 24 saat sonra **sabit** düşüyordu | (a) SL %2'ye düşüyor → risk profili sessizce değişiyor; (b) R-çarpanı kayboluyor → **kazanan işlemler terfi matematiğinden düşüyor**, yani sistematik **ters seçim bias'ı** | ✅ kayıt pozisyon kapanınca düşer (`closed_pnl` üzerinden R bir kez yazılır); zaman filtresi yalnız pozisyonsuz artıkları toparlar |
| 32 | `sync_positions_from` "boş dizi = hata" sayıyordu | tüm pozisyonlar gerçekten kapandığında harita **ömür boyu donuyor** → `open_now` tavanı hiç açılmıyor, tahsis hep kapalı. Testnet hep pozisyonlu olduğu için görülmemiş | ✅ `null`/yok = hata, **geçerli boş dizi = flat**. Kapanan coinler döndürülüyor, stop kayıtları da düşüyor. `parse_positions` saflaştırıldı → 4 test |
| 33 | **`refresh_orders` stop'ları tanımıyordu**: `isTrigger`/`triggerPx` alanlarına bakıyordu, testnet `openOrders` yanıtı bu alanları **hiç göndermiyor** (`isTrigger: None` ham yanıtta doğrulandı) | `sl_orders` hiç dolmuyor → `sync_exchange_sl` her döngü "eksik stop" sanıyor → **aynı pozisyona ikinci stop asılıyor** (ölçüldü: GMT oid=…61867 + …165356, LAYER …61871 + …166722 aynı anda borsada) | ✅ kural değişti: **duran + `reduceOnly` emir stop'tur** (bot'un tüm diğer reduce-only emirleri IOC, IOC dinlenmez → deftere girmez). `dedupe_sl` aynı coin'de en yeni oid'i bırakıp eskilerini iptal ediyor — açılışta da çalışıyor |
| 34 | **Asıl sızıntı**: `tokio::spawn` `&mut self` yakalayamıyor, dolayısıyla on-fill yolunda kurulan stop'un oid'i `sl_orders`'a **hiç yazılmıyordu** (sadece loga düşüyordu); `sl_armed_at` de yazılmadığı için grace kapısı da tutmuyordu | grace + refresh_orders düzeltmeleri boşa çıktı, çift stop 5 dakikada bir tekrarlıyordu | ✅ spawn **öncesi** `sl_armed_at` iyimser yazılıyor + oid `sl_tx` kanalıyla döner, `run_cycle` başında `drain_sl_results()` `sl_orders`'a yazar |
| 35 | **Trailing stop: `ensure_sl` idempotent dönüşü her şeyi sıfırlıyordu** — `Ok(())` hem "kuruldu" hem "zaten var" demekti, çağıran `cur_sl_px`'i sabit giriş stop'una geri yazıyordu | trailing hiç ratchet olmadı (ölçüldü) | ✅ `ensure_sl -> Result<bool>`, `cur_sl_px` yalnız yeni kurulumda güncelleniyor |
| 36 | **REST `limitPx` stop fiyatı değil** — `place_trigger(is_market)` gövdeyi `trigger×1.05`/`×0.95` yolluyor, REST yalnız `limitPx` döndürüyor | restart sonrası stop seviyesi %5 yanlış okunur → stop **gevşetilir**, koruma sessizce zayıflar. Yön ters karıştırılırsa %10.5 → stop ters yöne kayar | ✅ `/1.05` (alış) `/0.95` (satış) ile geri çevriliyor + tick ızgarasına oturtuluyor (`OrderInfo.asset`). Canlı kanıt: LAYER `0.07868414` ↔ REST `0.082614` |
| 37 | ⚠️⚠️ **STOP, DOLUM FİYATINDAN HESAPLANMIYORDU — TÜM KAYIPLARIN KÖK NEDENİ** | Karar anındaki **defter** fiyatı + risk mesafesiyle mutlak stop fiyatı üretiliyordu. IOC kayması dolumu %0.9 daha kötü getirdi → kısa pozisyonun stop'u dolumun **0.9% ALTINDA** kaldı → zaten piyasada olduğu için **1.1 saniyede tetiklendi** (ölçüldü: GMT short 0.009083 dolum → stop 0.0090015 → 1.1 sn'de tetik, −$0.21) | Her kısa pozisyon fiilen kendi stop'unu tetiklerdi. Önceki 4 kayp örneği ve "1-4 dk'da stop'lanma" gözlemi **muhtemelen aynı hata** | ✅ `pending_sl` artık **risk mesafesi** taşıyor; stop `dolum_px ± mesafe` ile kuruluyor + yan taraf sert kapısı (min %0.25). `entry_attr.entry_px` her döngü REST `entryPx` ile düzeltiliyor. 4 test |
| 38 | `explore_max_usd = $150` **ölü konfig** (havuz = bütçenin ≤%5'i = $15) | dokümanda tavan gibi görünüyor, hiç erişilemez | ✅ dokümanda ölü konfig olarak işaretlendi |

---

## DÜRÜST BORÇ LİSTESİ (karar vermeden önce bilinmeli)

1. **S19 look-ahead düzeltilmedi** → canlı ağırlıklar biased backtest'ten geliyor.
   Bu, "neden 2 strateji kazandı" sorusunun temelidir. **Öncelik 1.**
2. **Kâr yok** — hiçbir çıkış henüz R-çarpanı kaydetmedi. 0 kazanç, sistem
   doğruluğunu değil kârlılığını kanıtlamaz.
3. **Korelasyon kriz modu, VaR, funding EMA, makine-bazlı stale koruması yok.**
4. **Restart istatistik kaybı** (S20) — terfi kararları her restart'ta sıfırlanıyor.
5. **Keşif şeridi %5 bütçe tavanıyla** pratikte döngü başına 1 keşifçi
   finanse edebiliyor (bütçe $300 → havuz $15 → ⌊15/10⌋ = 1). Terfi (n≥30)
   için bu **çok yavaş**; `ENGINE_BUDGET_USD` veya tavan artırılmalı.
6. **Piramitleme tamamen kapalı** — açık pozisyon varken aynı coine giriş yok.
   Bilinçli karar (risk), ama trend stratejileri için maliyetli olabilir.
7. **Sinyal çıktısı hâlâ çok düşük.** ~55-79 oy → 0-2 aday (0.12 conviction
   kapısı). 2026-10-02 koşusunda 8 döngüde 4 pozisyon açıldı, 3'ü borsa stop'una
   takıldı (`closed<0`, ~-$0.57 toplam). 18 stratejiden yalnız 2'si eğitimi
   geçti; **kâr yok**. Motorun doğruluğu kanıtlandı, kârlılığı değil.

8. ~~Trailing stop uygulanmadı.~~ ✅ **UYGULANDI** (§8.3.15): SL ratchet
   (`tepe ∓ 1.5×ATR`, asla gevşemez), **TP sabit** (`tp_atr_mult = 3.0`).
   Kural: **önce yeni stop, sonra eskiyi iptal** (`Executor::replace_sl`).
   `TRAIL_ON` / `TRAIL_ATR_K` / `TRAIL_MIN_STEP` ile ayarlanır. 8 test.
9. **Trailing tepe (`peak`) restart'ta kaybolur** (bilinçli kabul, §8.3.14).
   *Ek — trailing stop'un seviyesi*: `trail.cur_sl_px` RAM'de, restart'ta
   boş olurdu → ilk döngüde stop **giriş ± ATR** seviyesine geri çekilir ve
   koruma zayıflar. ✅ Düzeltildi: `Executor::sl_last_px` borsadaki **gerçek**
   stop fiyatını tutuyor (`dedupe_sl` `openOrders`'tan, `ensure_sl`/`replace_sl`
   yerleştirmeden yazıyor) ve `tralstate` kurulurken ondan okunuyor.
10. **Trailing'in faydası henüz kanıtlanmadı.** `[MFE]` logu eklendi
   (MFE/MAE R cinsinden) ama mevcut 4 kayıp örneğin hepsi 1-4 dk içinde
   stop'landı (MFE ≈ 0) → mevcut veri trailing'in işe yaramayacağını söylüyor.
   Karar (tut/kapat/k değiştir) birkaç yüz işlem sonra verilmeli.

*Detaylı çözüm için: SISTEM-MIMARISI.md §8 (ENV), §8.0.2 (cap birimleri),
§8.1 (keşif şeridi), §8.2 (borsa emir kuralları), §8.3.x (bugfix'ler).*

---

## TEST İNVARIANT'LARI (0 panik yetmez)

Bu turun anahtar düzeltmesi: **"0 panik" hiçbir şey kanıtlamıyor.** Yaşanan
hataların hepsi panik vermeden sessizce kaybettiriyordu. Testler artık
invariant assertion içeriyor:

| İnvariant | Test | Eski davranış |
|---|---|---|
| `positions.len() ≤ TOP_N` | canlı log + `open_now` sayacı | 3 varken 4. giriş açıldı |
| tahsis ≤ `max_position_usd` | `tahsis_pozisyon_tavanini_asmaz` | $300 bütçe → $300 tahsis, risk reddi |
| tahsis ve risk **aynı** tavanı paylaşır | `AllocationConfig` ↔ `risk_manager` eşitlik | farklı tavanlar, sinyal boşa gidiyordu |
| geçerli boş `assetPositions` = flat | `bos_dizi_*` (2 test) | harita ömür boyu donuyordu |
| `szi` işareti doğru (short negatif) | `dolu_dizi_harita_kurar_*` | `Default` = 0 → tavan hiç işlemiyor |
| cloid `0x`+32 hex, deterministik | `cloid_16_hane…` (34 hane kontrolü) | 16 hane → canlıda HTTP 422 |
| stop grace penceresi | `suregre_kapisi_tekrar_kurmayi_engeller` | 5 dk'ta ikinci oid |
| trailing asla gevşetmez | `trailing_asla_gevsetmez` | — |
| trailing long/short yönü | `long_trailing_yukarı…`, `short_trailing_asagi…` | — |
| min adım eşiği | `min_adim_esigi_kucuk_iletlemeyi_bloklar` | her küçük iyileştirmeye 2 POST |
| ilk adım nötr | `ilk_adim_zaten_gevsetmez…` | — |
| MFE/MAE işareti (long+short) | `mfe_mae_r_cinsinden…`, `short_mfe_tersi…` | MAE **pozitif** hesaplanıyordu |
| TP'nin neden trailing olmadığı | `tp_sabit_kalir_trailing_yok` (2:1→1:1) | doküman "3:1" diyordu, gerçek 2:1 |
| stop fiyatı `limitPx`'ten geri çevrilir | `rest_limit_px_ters_cevirilir…` | %5 kayık → stop gevşer |
| IOC kayması stop'u yan tarafa taşımaz | `ioc_kaymasi_stopu_yan_tarafa_tasimaz` | %0.9 kayma → 1.1 sn'de tetiklenen kısa |
| %9 kayma altında stop lehli | `kayma_ne_kadar_buyuk…` (0/0.5/1/3/9%) | — |
| risk ≈ 0'da min mesafe | `risk_mesafesi_cok_kucukse…` | ATR âmı → stop dolumun üstüne binerdi |
| bozuk risk (negatif/NaN/MAX) | `bozuk_risk_degerinde_de…` | ters yönlü emir = kâr garantisi |
| yön karıştırılırsa | `yon_karistirilirsa…` | %10.5 → stop ters yöne kayar |

Toplam: engine **56 test** (36 taban + 4 senaryo + 2 cloid + 2 stop-px +
4 sl-side + 8 trailing), strategy 4, workspace temiz.
