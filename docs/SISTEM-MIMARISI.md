# SİSTEM MİMARİSİ — hyperliquid-trading-bot

Bu doküman sistemin mevcut çalışma mimarisini, katmanlarını, veri akışını,
risk kontratlarını, execution yaşam döngüsünü ve öğrenme modelini tanımlar.
Kaynak: repository'deki kod. Her bölüm kodla birebir senkronizedir.

---

## 1. Sistem amacı ve sınırları

Sistem, Hyperliquid perpetual borsasında çalışan algoritmik bir işlem botudur.
Tek amacı vardır: gerçek kapanmış işlemlerden öğrenen, kontratlarla korunan
bir işlem motorunu 100 gerçek kapanmış işlemlik öğrenme döneminde koşturmak.

Sınırlar:

- Bot bir **motor**dur: karar, tahsis, emir, çıkış, öğrenme. Piyasa tahmini
  iddiası yoktur; her karar ölçülebilir kontratlara bağlıdır.
- Öğrenme dönemi TESTNET'tir. Canlı endpoint'e karşı başlatma **yapısal
  olarak engellenir** (`assert_testnet_endpoint`).
- `ready_for_live` otomatik canlı işlem demek değildir (bkz. §26).
- Mainnet konfigürasyonuyla başlatılırsa öğrenme dönemi mantığı devre dışıdır;
  canlıya geçiş yalnız operatörün ayrı, bilinçli adımıdır.

## 2. Genel mimari

```text
┌────────────────────────────────────────────────────────────┐
│ bot (engine/src/bin/bot.rs)                                │
│  NTP → ENV → epoch → risk recovery → WS ingestor → döngü   │
└──────────────┬─────────────────────────────────────────────┘
               │ her ENGINE_CYCLE_SECS (300 sn)
┌──────────────▼─────────────────────────────────────────────┐
│ engine_loop                                                │
│  tarama → ensemble → rejim → tahsis → EV → risk →          │
│  emir → dolum → çıkış → R → tracker → canlı ağırlık        │
└──────┬─────────────┬───────────────┬───────────────────────┘
       │             │               │
   data katmanı  executor       kalıcılık (config/*.json,
   (WS/REST)     (borsa I/O)    ClickHouse/Redis opsiyonel)
```

Crate bağımlılık yönü: `math` ← `strategy` ← `engine`; `api` ← `data`,
`engine`; `utils` hepsine hizmet eder. Motor mantığı `strategy` veya
`data` içinde yaşamaz.

## 3. Runtime veri akışı

```text
Hyperliquid WS (shard'lı)                 Hyperliquid REST
    │ Ingestor                                 │ backfill / snapshot
    ▼                                          ▼
StreamEvent ──► Aggregator (tick→mum) ──► Candle serisi
    │                                          │
    ├──► MarketCtx (funding/OI/mark/oracle)    │
    ▼                                          ▼
        MarketInput (3 temel modüle TEK girdi sözleşmesi)
                       │
                       ▼
        Strategy Layer (panik-izole evaluate)
                       │
                       ▼
        Ensemble (skor + yön + exit kanalı)
                       │
                       ▼
        Regime Gate (Rejim Tespiti kapısı)
                       │
                       ▼
        Allocation (conviction → Candidate + SL/TP kontratı)
                       │
                       ▼
        EV / Cost Gate (TEK maliyet modeli)
                       │
                       ▼
        Risk (pre_trade kapıları + EntryGate)
                       │
                       ▼
        State Machine (Action) → batch WireOrder → borsa
                       │
                       ▼
        Fill (orderUpdates/userEvents) → Reconciliation
                       │
                       ▼
        Position State → Exit (SL/TP/trailing/ExitNow)
                       │
                       ▼
        Realized R → StrategyTracker → Live Weights
```

## 4. Crate / modül sorumlulukları

| Crate | Sorumluluk |
|---|---|
| `api` | Hyperliquid I/O façade: imza (EIP-712), exchange/info endpoint, WS, hata taksonomisi, wire yuvarlama. Bot mantığı İÇERMEZ. |
| `data` | Piyasa verisi hattı: WS ingestor, tick→mum aggregator, market context, storage soyutlaması, evren tek kaynağı. |
| `strategy` | `Strategy` trait'i + tek temel modül (MRE) + ensemble + rejim + `InfoCluster` + `exit_intent`. |
| `engine` | Motor: döngü, executor, allocation, risk, durum makinesi, tracker, epoch. |
| `math` | Saf deterministik sayısal çekirdek: göstergeler, vol modelleri, linalg/OLS, istatistik testleri, portföy metrikleri. |
| `utils` | Logger + NTP saat senkronu. |
| `scripts` | Operasyon: `start_bot.sh` / `start_bot_mainnet.sh` / `watchdog.sh`. |
| `bin/probe_px` | Fiyat/lot hassasiyet keşif aracı (pozisyon üretmez). |
| `bin/trail_report` | `trade_exits.jsonl` offline analitiği (MFE/MAE, çıkış dağılımı). |

## 5. Market data pipeline

- **Ingestor**: evreni shard'lara böler (shard başına ~160 kanal), her shard
  için ayrı WS açar. Ham kanallar (`trades / l2Book / activeAssetCtx /
  orderUpdates / userEvents`) `StreamEvent`'e çevrilir. Evren DIŞARIDAN
  verilir; sessiz kırpma yoktur.
- **Aggregator**: tick → kapanmış mum + canlı son bar; taker akışı toplar.
  Parse edilemeyen/finite olmayan mum **saklanmaz** — o bar düşürülür;
  geçersiz veri gerçek sıfır fiyat gibi sinyale girmez.
- **MarketCtx**: funding / OI / mark / oracle / prevDayPx serilerini WS +
  REST'ten birleştirir. Alan parse edilemezse son bilinen değer korunur
  (UNKNOWN ≠ geçerli sıfır). HL `funding` alanı **saatlik** orandır;
  dönüşüm strateji katmanındadır. OI serisi 1500 ölçüme kadar tutulur (≈3h)
  — bar-penceresinde ΔOI ölçümü için.
- **Storage**: `Storage` trait — ClickHouse (üretim arşivi, ReplacingMergeTree
  dedup, resume imleci), Memory (test/fallback), Redis (sıcak mids/ctx).
  URL boşsa sistem Memory ile çalışmayı sürdürür (indirimli mod loglanır).
  Bot canlı yolda CH'yi kullanmaz; CH yalnız arşiv/backfill içindir.
- **Backfill**: sayfalı geçmiş mum/funding pompası; motorun ısınma penceresi
  (`ENGINE_HIST_BARS`) REST snapshot ile açılır.
- **Stale koruması**: `LocalBook::is_stale/is_sane` — bayat/bozuk defter
  sinyale ve emre gitmez.

## 6. Trading universe

`TradingUniverse` **tek kaynaktır**: `UniverseSpec::parse(ENGINE_COINS)` →
`all` (tüm aktif evren, ~158 coin) / `Exact` (birebir liste; eksik coin =
hata) / `TopByVolume(N)` (deterministik kırıcı: volume desc, isim asc).

`verify_layers` açılışta engine/ingestor/risk/WS katmanlarının aynı listeyi
kullandığını doğrular; uyuşmazlıkta **açılış durur**.

## 7. Strategy pipeline

- `Strategy` trait'i: `name()`, `evaluate(&MarketInput) -> Signal`,
  `status()`, `exit_intent(&ExitCtx) -> Option<ExitIntent>`.
- `MarketInput`: kapanmış mumlar + canlı son bar + L2 defter (`Option` —
  yoksa `None`, sahte sıfır DEĞİL) + ctx (OI/funding) + portfolio kanıtı +
  equity + evren getirileri.
- Tek temel modül `registry()`'de tanımlıdır (MRE). `evaluate` panik-izole çalışır
  (bir stratejinin paniki döngüyü düşürmez; Skip'e döner).
- `Signal`: Long/Short/Hold/Skip/Exit — `None` gösterge asla sahte oy
  üretmez (gösterge yok = katkı yok).
- `status()`: `Active / Auxiliary` — rapor dürüstlüğü. Girdisi yapısal
  olarak sağlanamayan stratejiler (IV feed, temel veri, parent-order
  planner) registry'de YER ALMAZ — boş koltuk doldurmaz, sahte yön
  üretmez.

## 8. Regime detection

`Rejim Tespiti` ADX + Hurst + vol rejiminden `RegimeGate` üretir. ADX/Hurst
hesaplanamıyorsa strateji **Skip** üretir; `classify` `None` dönerse motor
güvenli **Open** kapısına düşer — sahte rejim değeri (0/0.5) üretilmez.
TrendBoost rejim filtresi **yalnız skor çarpanıdır**: `dir`/eşik kararı
combine'da verilmiştir, rejim katmanı onu yeniden hesaplamaz.

## 9. Ensemble / voting

`Ensemble::combine` strateji oylarını ağırlıklı skora indirger:
`ConsensusSignal { dir, score, contributors, note, exit_requested,
exit_contributors }`. `|skor| < ENGINE_MIN_SCORE` → nötr.

**Çıkış kanalı ayrıdır**: `Signal::Exit` skora nötr etki eder; en az bir
Exit oyu varsa `exit_requested=true` taşınır. Motor yalnız AÇIK pozisyonda
bu bayrakla `ExitNow` yürütür; Flat'ta hiçbir emir üretmez.

## 10. InfoCluster ve breadth

Modüller aynı temel girdilerin formülasyonları olabilir; genişlik ödülü
**ham oy sayısına göre verilmez**. `strategy::info_cluster(name)` tek
kaynaktır — 8 bilgi kümesi taksonomisi durur; şu an eşlenen modül:

```text
MeanReversion (MRE — De Bondt-Thaler Z + GARCH σ + CS rank + yönlü infaz mumu)
```

Tahsisin conviction genişliği **ayrı küme sayısından** gelir:
`breadth = 1 + 0.1×(ayrı_küme−1)` (0.5..1.5 clamp). Aynı kümeden oylar
genişlik ödülünü çoğaltmaz. Tek modüllü registry'de breadth = 1.0 (etkisiz)
— mekânizma çoklu modül geri geldiğinde devreye girer.

## 11. Allocation

`AllocationManager::allocate`:

1. Aday filtresi: yönlü + mid > 0 + min conviction.
2. Conviction: `share × breadth × strength / risk` (keşif oyu indirimi ile).
3. Tahsis: conviction² kare ağırlıklı bütçe dilimleri; **pozisyon başına
   tavan** (`max_position_usd`).
4. SL/TP kontratı karar anında türetilir: `sl = entry ± sl_atr_mult×ATR`;
   `tp = entry ∓ max(tp/sl×risk, 2×risk)` — TP **gerçek risk mesafesinin**
   en az iki katı kapılıdır.
5. MMT voter'lı adaylar kanonik TUNE çarpanını (TP=5N=3.33R) taşır;
   diğer modüller motor tabanını (TP=3.0=2R) korur. Formül AYNI — yalnız
   çarpan modülden gelir.
6. Keşif şeridi: kanıtı yetersiz (n < full_n) stratejiler küçük lotla
   sınanır; keşif havuzu bütçe×%5'i aşamaz, taban havuzu aşamaz;
   finanse edilebilir keşifçi = `min(N, ⌊havuz/$10⌋)`.

## 12. Cost / EV gate

`CostModel` **TEK** maliyet kaynağıdır: taker fee (4.5 bps) + taban
slippage (5 bps) + `EV_MIN_DIST_BPS` (75). Kural:
`min_risk_dist_bps = max(75, 3×round_trip_cost_bps)`. EV kapısını geçmeyen
aday tahsis edilmez. Yürütme fiyat bandı aynı modelden gelir; operasyonel
taban `EXECUTION_BAND_FLOOR_BPS=50` ince defterde dolum kaçışını önler.
Motor içinde ad-hoc ikinci maliyet modeli yoktur.

Spread-gate spec formülü `max(75, 3×Spread + TakerFee)` mevcut gate'in
**alt sınırıdır** — mevcut gate (spread + slippage + iki bacak) daha sıkı.

## 13. Risk architecture

`RiskManager` — kalıcı risk hafızası + emir öncesi kapılar.

- Kalıcı state: `peak_equity`, `current_equity`, `halted`
  (`RISK_STATE_PATH`, atomik tmp+rename; boot'ta yüklenir).
- **Peak = yalnız SETTLED equity**: açık pozisyonlu (unrealized) equity
  spike'ı peak'i şişiremez (IO vakası: MFE anı 66.9→76.9 kalktı).
  Drawdown kontrolü her güncellemede mark equity ile yapılır (korumacı).
- Drawdown: peak'e göre hesaplanır; restart drawdown hafızasını sıfırlamaz.
- Kapılar: pozisyon notional tavanı, toplam notional, kaldıraç, minimum
  equity, max açık pozisyon sayısı, drawdown limiti.
- İhlal → `RiskViolation`; emir gönderilmez. Kapı EV/tahsis kapısından
  bağımsızdır (savunma derinliği).
- `is_halted` yalnız operatörle `resume` olur; otomatik açılış yoktur.

## 14. EntryGate / risk-lock

```text
entries_allowed = health_ok && !risk_locked
```

`entries_allowed` **alan değildir** — `EntryGate`'ten türetilir ve elle
yazılamaz (kaynak taraması testi korunur). Kaynaklar:

- `health.on_error` taksonomisi: yalnız **karar-kritik** hatalar sayılır
  (WS reconnect / geçici REST timeout / 429 sayılmaz); eşik
  `ENGINE_CRIT_THRESHOLD` (5).
- WS fırtınası (>10 kopma/5 dk) → `ExitsOnly`: kill yok; çıkışlar çalışır,
  girişler kapalıdır.
- `risk_locked` drawdown haltden gelir; sağlığa dönünce kilit kendiliğinden
  kalkar (tek denklem — iki bayrak).

## 15. Order construction

- `WireOrder` + `make_cloid`: **0x + 32 hane hex (128-bit)**; aynı
  (coin, fiyat, boyut, döngü) için aynı anahtar → retry idempotency.
  Kısa/16 hane gönderim API 422 ile reddedilir.
- Girişler Ioc; çıkışlar reduce-only.
- Fiyat/boyut borsa kurallarına göre yuvarlanır (`round_5sig`, `szDecimals`,
  `$10` minimum notional); bazı coinler 6 haneyi reddeder — ızgara bir hane
  azaltarak dener.
- TP ve SL **ayrı cloid tuzu** alır (`TP_CLOID_SALT` / SL tuzu) — TP emri
  SL dedupe'sine yakalanamaz.
- Emirler **TEK batch POST** ile iletilir (`batch_execute`); `place_limit`
  adaptif tick keşfi kullanmaz (6 hane reddeden coinlerde kapanabilir).

## 16. Execution lifecycle

```text
State Machine Action
   │ Place / ExitNow / Cancel
   ▼
batch WireOrder ──► ExchangeOutcome
   │                    │
   │        SUCCESS (resting_oid / filled)
   │                    │ ERROR → RetryPolicy (4xx retry YOK)
   │                    │ UNKNOWN (ağ zaman aşımı — borsada işlenmiş
   │                    │   OLABİLİR) → reconciliation zorunlu
   ▼                    ▼
orderUpdates / userEvents (WS) ──► FillEvent (is_tp_fill/is_sl_fill oid ile)
   │
   ▼
EntryAttr::apply_real_fill (REST entryPx) → SL/TP dolum fiyatına kalibre
```

## 17. Fill reconciliation

- **Dolum kaynakları**: WS `orderUpdates` (anlık) + `userEvents` (fill) +
  periyodik REST recon (30 sn). WS koparsa kapanış kaydı REST taramasından
  yine yazılır.
- **UNKNOWN emir sonucu**: borsada bulunursa oid bağlanır; bulunamazsa
  yalnız `oid==0` pending girişte Flat revert serbesttir — sessiz state
  değişimi yoktur.
- **Açılış/kapanış ayrımı** `dir` alanından yapılır (`Open Long/Short`,
  `Close Long/Short`, `Long > Short/Short > Long` = ters çevirme → açılış);
  `closedPnl` KULLANILMAZ (break-even kapanışta 0'dır).
- `closed_pnl` haritası closing fill'i `dir`'e bakmadan taşır — break-even
  kapanış kaydı kaybolmaz.
- Borsa `assetPositions`'ı **boş dizi** döndürürse harita SIFIRLANIR
  (tüm pozisyonlar kapanmıştır); anahtar yok/null ise hatadır ve önceki
  harita korunur.

## 18. Position state machine

```text
        Place                fill                ExitNow/force_exit
Flat ─────────► Pending ─────────► InPosition ─────────────► PendingExit ──► Flat
  ▲                │  reject → Flat  ▲    borsa TP/SL/external
  │                │                 │    close → recon → Flat
  └────────────────┴─────────────────┘
```

`PosState`: `Flat / Pending{side,oid,remaining,ts} / InPosition{side,entry,
size,peak} / PendingExit{oid,side}`. Garantiler: Pending'te aynı yön →
Nothing (çift emir yok); Flat'ta çıkış emri ÜRETİLMEZ; bilinmeyen oid →
no-op; revert yalnız oid==0 pending'de.

## 19. TP / SL / trailing

- **SL**: giriş dolumunda reduce-only trigger (`tpsl="sl"`) borsaya asılır.
  Stop fiyatı **gerçek dolumdan** hesaplanır — IOC kayması mesafe kontratını
  bozamaz. Sert yan kapısı: stop dolumun hatalı tarafına konulamaz.
- **TP**: reduce-only trigger-LIMIT (`tpsl="tp"`); fill →
  `ExitReason::TakeProfit`. TP **gerçek kapanış kaynağıdır**.
- **Trailing**: yalnız SL ratchet'lenir (`TRAIL_ATR_K×ATR`,
  `TRAIL_MIN_STEP` — gevşetme yasak). `replace_sl` önce YENİYİ kurar,
  sonra öncekini iptal eder; yerleştirme başarısızsa önceki stop ayakta
  kalır (korumasız pozisyon yok).
- **Dedupe**: aynı coin'de birden fazla stop = yetim; en yeni oid tutulur,
  öncekiler iptal edilir. TP/SL ayrımı `is_our_tp_row` fiyat-yakınlığıyla
  yapılır — dedupe kendi TP'sini iptal edemez.
- **Zaman damgası grace kapısı** (`TS_GRACE_MS`): reconnect'te harita boşsa
  aynı stop tekrar kurulamaz.
- **Stop fiyatı REST'ten ters hesaplanır**: `openOrders` yalnız `limitPx`
  döndürür; `limitPx/1.05` (satış-stop) veya `/0.95` (alış-stop).

## 20. Exit ownership / ExitReason

Çıkış kararlarının sahibi bellidir:

```text
ExitReason: Unknown | StopLoss | TakeProfit | TrailingStop |
            SignalExit | EmergencyFlatten | Manual
```

- **Strateji sahipliği**: çıkış KURALLARI `Strategy::exit_intent(&ExitCtx)`
  API'sinde yaşar (Kesit rank kayması, Turtle S1 10-bar adverse). Motor kural
  fonksiyonlarını doğrudan çağıramaz (kaynak taraması testi).
- **Ensemble çıkışı**: `exit_requested` → `force_exit` (Flat'ta emir yok).
- **Borsa sahipliği**: SL/TP trigger fill → oid eşleşmesiyle
  `StopLoss`/`TakeProfit`.
- Her kapanış TAM BİR `ExitReason` alır; kaynak-eşleşme testleriyle
  korunur (TP fill → TakeProfit; SL fill → StopLoss; karışım yasak).

## 21. Learning / tracker

`StrategyTracker` her strateji için n / win / PF / ΣR tutar ve ağırlık
üretilir:

- `n < LEARN_MIN_N (10)` → ağırlık DONMUŞ `1.0` (kanıt yetersiz → öldürme yok).
- `min_n ≤ n < full_n` → kısmi kurallar + keşif kuyruğu.
- `n ≥ LEARN_FULL_N (30)` → tam kurallar (win < %35 → 0) + keşif dışı.

Yalnız **gerçek kapanış R** kaydı girer: TP/SL/exit fill →
`record_outcome_r(voters, ts, R)`. Sentetik ±1R, backtest satırı, fake
win-loss kaydı yazılamaz (dosya formatı `origin:"live"` + `rs` gerçek R
history ile korunur). PnL çözülemeyen kapanış tracker'a **0R YAZILMAZ**
(TradeExit + sayaç kaydolur; öğrenme kanıtı oluşmaz).

## 22. Live weight persistence

- Etkin ağırlık = tracker türevi × canlı snapshot birleşimi
  (`merge_effective_weights`); canlı dosya yüklenirse **birebir**
  uygulanır — bir sonraki uygulamada sessizce 1.0'a ezilemez.
- Dosya: `ENSEMBLE_LIVE_WEIGHTS_OUT` — atomik tmp+rename, 300 sn'de bir dump.
- Okuma: `ENSEMBLE_WEIGHTS_IN` — **yalnız `origin:"live"`** kabul edilir;
  `origin` olmayan / backtest-üretimli dosya reddedilir; bozuk sayı/sema →
  **AÇILIŞ BAŞARISIZ** (sessiz cold-start = öğrenme kaybı); dosya yok →
  nötr cold-start (3 × 1.0).
- `schema_version: 2`, gerçek R history saklanır; restart'ta birebir yüklenir.

## 23. Learning epoch

`LearningEpoch` (`LEARNING_EPOCH_PATH`) testnet deneyinin kaydıdır:

```text
TESTNET ──► cold start ──► 100 gerçek kapanmış işlem ──► ready_for_live
```

- `status`: `Learning → ReadyForLive` (100. kapanışta geçer).
- `completed_trade_count` = **kapanmış pozisyon** sayısı — sinyal, emir veya
  fill sayısı değildir. `1 gerçek kapanmış pozisyon = 1 completed trade`;
  her kapanış tam BİR kez sayılır (tid dedup + EntryAttr tek-yazım).
- Sayaç kalıcıdır; restart'ta kaldığı yerden devam eder. Bozuk epoch
  dosyası **açılışı durdurur** (sessiz sıfırlama yok).
- `TESTNET_FRESH_START=1`: yalnız İLK çalıştırmada sıfırlar — tracker,
  canlı ağırlık, risk hafızası, epoch temizlenir; açık emirler iptal,
  pozisyonlar flatten; `trade_exits.jsonl` arşivlenir. Sonraki
  restartlarda ENV kalsa bile state KORUNUR.
- Ortam-farkında kurucu: mainnet'te `environment:"mainnet"` mührü basılır
  (`LearningEpoch::new_for_environment`).

## 24. Restart / recovery

- **Pozisyon devri** (`ENGINE_FLATTEN=0`): borsadaki pozisyonlar recovery
  ile makinelere bağlanır; TP/SL emirleri borsada durur ve cloid tuzu
  deterministik olduğundan **körlemesine ikinci emir atılmaz** — oid
  GERİ YÜKLENİR.
- Devralınan pozisyon attr'sızsa **sentetik EntryAttr** kurulur: gerçek
  entry/side/size + %2 risk fallback + voters BOŞ (kapanış sayılır,
  tracker'a atıf kanıtı YAZILMAZ — sahipsiz stratejiye kredi yok).
- **Boot sırası kontratı**: state recovery → pozisyon haritası dolu →
  yetem temizliği → çift-stop dedupe. Yetem temizliği harita boşken
  koşarsa gerçek pozisyonun TP/SL'si "yetim" sanılıp iptal edilir;
  sıra testlerle korunur.
- Risk hafızası boot'ta yüklenir; drawdown koruması süreklidir.

## 25. Testnet learning protocol

- Bot yalnız `NETWORK=testnet` + testnet endpoint'leriyle başlar;
  `assert_testnet_endpoint` canlı API/NETWORK ile başlatmayı emir atılmadan
  reddeder. TESTNET_FRESH_START canlı endpoint'te çalışamaz.
- Öğrenme dönemi tek deneydir: aynı konfigürasyonla (stratejiler, eşikler,
  risk limitleri, evren) 100 gerçek kapanış toplanır. Deney sırasında
  parametre değişikliği yapılmaz.
- `cargo test --workspace`: **311 passed / 0 failed** (engine 227,
  data 32, api 13, strategy 39). Release build ayrı derlenir.

## 26. Mainnet promotion boundary

```text
Testnet learning (100 completed trades)
        ↓
ready_for_live          ← epoch durum yazımı
        ↓
operator decision       ← İNSAN kararı; otomatik geçiş YOK
        ↓
mainnet promotion       ← ayrı operasyon; bot kendisi mainnet'e geçmez
```

`ready_for_live != automatically live`. Sistem mainnet konfigürasyonuyla
başlatılırsa öğrenme dönemi mantığı devre dışıdır; canlıya geçiş yalnız
operatörün ayrı, bilinçli adımıdır.

## 27. Persistence files

| State | Dosya | Amaç | Restart'ta |
|---|---|---|---|
| Tracker | `config/track_state.json` | n/win/PF/ΣR + gerçek R history | yüklenir (atomik) |
| Live weights | `config/ensemble_weights_live.json` | `origin:"live"` ağırlık snapshot | doğrulanır + yüklenir |
| Learning epoch | `config/learning_epoch.json` | dönem + kalıcı işlem sayacı | korunur |
| Risk state | `config/risk_state.json` | peak_equity + halted | yüklenir |
| Tick cache | `config/tick_cache.json` | coin→fiyat hassasiyeti | yüklenir |
| Trade exits | `config/trade_exits.jsonl` | kapanmış işlem arşivi (append) | append/recovery |

Tümü atomik yazılır (`.tmp` + rename). `FRESH_START` tracker/weights/
epoch/risk'i temizler, `trade_exits.jsonl`'i arşivler. Testnet state'i
mainnet state'inden ayrı tutulur: mainnet operasyonu kendi dosya
yollarıyla (`*_PATH` ENV) yapılandırılır; dönem dosyası mainnet'te
anlamsızdır.

## 28. ENV configuration

`.env.example` kod ve operasyon scriptiyle senkrondur (gizli değerler
`.env`'de, repoya girmez):

| ENV | Varsayılan | Anlam |
|---|---|---|
| `NETWORK` | `testnet` | `testnet`/`mainnet`; testnet epoch canlı endpoint'i reddeder |
| `HYPERLIQUID_API_URL` / `_WS_URL` | testnet uçları | REST + WS |
| `HYPERLIQUID_PRIVATE_KEY` / `API_WALLET_ADDRESS` / `USER_ADDRESS` | — | imzalayan agent + master hesap adresi |
| `ENGINE_INTERVAL` | `3600` | mum aralığı (sn) |
| `ENGINE_CYCLE_SECS` | `300` | döngü periyodu |
| `ENGINE_COINS` | `all` | evren spesifikasyonu (tek kaynak) |
| `ENGINE_L2BOOK_TOP_N` | `10000` (≈tam evren) | SMC-L L2 book aboneliği: hacme göre ilk N coin; 0 = kapalı |
| `ENGINE_HIST_BARS` | `4400` | ısınma penceresi |
| `ENGINE_BUDGET_USD` | `300` | döngü bütçesi |
| `ENGINE_TOP_N` | `30` | eşzamanlı pozisyon tavanı |
| `ENGINE_MIN_SCORE` | `0.30` | ensemble eşik |
| `ENGINE_LEVERAGE` | `5` | hesap kaldıracı |
| `ENGINE_FLATTEN` | `0` | açılışta düzleşme (0 = devral) |
| `ENGINE_EXPLORE` | `1` | keşif şeridi |
| `ENGINE_CRIT_THRESHOLD` | `5` | kill-switch eşiği (karar-kritik) |
| `TRAIL_ON` / `TRAIL_ATR_K` / `TRAIL_MIN_STEP` | `1 / 1.5 / 0.25` | trailing stop |
| `LEARN_MIN_N` / `LEARN_FULL_N` | `10 / 30` | öğrenme pencere eşikleri |
| `TRACK_STATE_PATH` | `config/track_state.json` | tracker kalıcılığı |
| `ENSEMBLE_WEIGHTS_IN` / `_OUT` | `config/ensemble_weights_live.json` | canlı ağırlık okuma/yazma |
| `ENGINE_TICK_CACHE` | `config/tick_cache.json` | fiyat hassasiyeti öğrenimi |
| `TRADE_EXITS_PATH` | `config/trade_exits.jsonl` | kapanış arşivi |
| `TESTNET_FRESH_START` | (boş) | 1 = yalnız ilk çalıştırma reset |
| `LEARNING_EPOCH_PATH` | `config/learning_epoch.json` | dönem kaydı |
| `RISK_STATE_PATH` | `config/risk_state.json` | kalıcı risk hafızası |
| `EV_TAKER_FEE_BPS` / `EV_SLIPPAGE_BPS` / `EV_MIN_DIST_BPS` | `4.5 / 5.0 / 75` | tek maliyet modeli |
| `MMT_TP_ATR_MULT` | `5.0` | MMT voter'lı aday TP çarpanı (ATR×) |
| `CLICKHOUSE_URL` / `REDIS_URL` | (boş → Memory) | opsiyonel kalıcılık |
| `LOG_LEVEL` / `RUST_LOG` | `info` | log seviyesi (LOG_LEVEL öncelikli) |

`start_bot.sh` operasyon değerlerini export eder (bütçe 300, döngü 300 sn,
1h mum, 4400 bar, FLATTEN=0) ve botu **timeout'suz süresiz** çalıştırır.
`start_bot_mainnet.sh` mainnet onayı + CH kontrolü + watchdog ile çalışır.

## 29. Failure modes / safe behavior

| Arıza | Davranış |
|---|---|
| WS kopması | otomatik reconnect (5 sn); shard'lı; fırtınada ExitsOnly |
| REST 429/timeout | geçici sınıftır; kill-switch saymaz; retry policy (4xx retry yok) |
| UNKNOWN emir sonucu | borsada işlenmiş OLABİLİR → recon zorunlu; sessiz state değişimi yok |
| Bozuk ağırlık dosyası | açılış BAŞARISIZ (sessiz cold-start yasak) |
| Bozuk epoch dosyası | açılış DURUR (sessiz sayaç sıfırlama yasak) |
| Bozuk risk state | yükleme reddedilir; fresh başlangıç loglanır |
| Bayat/bozuk defter | sinyale ve emre gitmez (`is_stale/is_sane`) |
| Geçersiz mum/ctx alanı | mum düşürülür; alan son bilinen değeri korur — sahte sıfır yok |
| Gösterge None | strateji Skip — sahte yön yok |
| ADX/Hurst hesaplanamaz | rejim Skip; `classify` None → güvenli Open |
| Drawdown halt | yalnız operatör `resume`; state kalıcı |
| Yetim/çift stop | boot dedupe + grace kapısı + dedupe_sl (en yeni oid kalır) |
| Kill-switch | EmergencyFlatten; restart exponential backoff (30..300 sn) |
| Watchdog | PIDFILE tabanlı; disable lock ile operatör durdurma talebi |

## 30. Test architecture

```text
cargo test --workspace  →  311 passed / 0 failed
engine 227 · data 32 · api 13 · strategy 39
```

- **Kontrat testleri**: EntryGate denklemi (kaynak taraması), exit sahipliği
  (engine doğrudan kural çağrısı yasak), TP≥2R, cloid şeması, dedupe TP/SL
  ayrımı, boot sırası, drain-sırası, UNKNOWN→recon.
- **Golden path**: strateji → ensemble → rejim → tahsis → risk → emir
  kontratı → fill → TP/SL → R → tracker zinciri ağsız gerçek tiplerle.
- **Mutasyon kanıtlaması**: persistence, epoch sayacı, risk hafızası,
  ağırlık validasyonu testleri mutasyonla doğrulanmıştır.
- **Data dürüstlüğü**: None kontratı, bozuk mum düşürme, ctx bayat-değer
  koruması, pencere-ΔOI testleri.
- **CI** (`.github/workflows/test.yml`): `cargo test --workspace` +
  `cargo build --release --workspace`. fmt/clippy adımları yoktur
  (bkz. Dürüst sınırlar).

## Dürüst sınırlar

- **Elliott** yapısal olarak long-only eğilimlidir; short kolu mevcut
  kontratta uygulanmaz (parametre normalizasyonu değiştirilmedi).
- **MMT ve SMC-L kaldırıldı**: MMT canlıda hiç işlem açlamadı (LEARN n=0);
  SMC-L'nin trade verisi Hyperliquid WS'den gelmiyordu (taker_ratio=None) ve
  zaten devre dışıydı. İki modül de git geçmişinde durur (silinme öncesi
  commit'ler). Aktif modül: MRE (STRATEGY_COUNT=1).
- **Kelly Sizing** ve **Funding Carry** motorun Sermaye/Risk katmanına
  (leverage_sizing + funding EMA/freeze) taşınmıştır; registry'den
  çıkarılmıştır (sinyal modülü değildir).
- **GARCH** kanonik Bollerslev(1986) GARCH(1,1) ölçümü + momentum
  kaplaması (yön kanıtı kaplamadır — notta açıkça işaretli; istasyonarlık
  kısıtı α+β<1 yüklüdür).
- **Backtest motoru yoktur**: ağırlıklar yalnız canlı kanıttan gelir;
  backtest formatı yalnız reddedilmek üzere tanımlıdır.
- CI fmt/clippy koşmaz; kalite kapısı test + release build'dir.
- Arşiv kalıcılığı (ClickHouse/Redis) opsiyoneldir; boşsa Memory fallback
  ile arşiv/backtest verisi birikmez. Bot canlı yolda CH kullanmaz.
- SMC-L kademeli kar al (partial profits) henüz uygulanmadı (exit_intent
  revival gerektirir — ayrı sprint).
