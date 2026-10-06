# SISTEM MİMARİSİ — hyperliquid-trading-bot

*Bu doküman 2026-10-05'te sıfırdan yeniden yazıldı; içerik koddan doğrulanmıştır.*

Testnet'te çalışan, testnet'te çalışan, algoritmik ticaret botu. Amaç **motorun doğruluğu** — kâr iddiası DEĞİL.

---

## 1. Sistemin işleyişi

```
 ┌──────────────────────────────────────────────┐
 Hyperliquid WS/REST → │ ingestor (shard'lı WS) → StreamEvent │
 │ ├→ aggregator (tick→mum, taker akışı) │
 │ └→ market_ctx (funding/OI/mark serileri) │
 └───────────────┬──────────────────────────────┘
 ▼
 engine_loop::run_cycle — her 300 sn'de bir
 ─────────────────────────────────────────────
 FAZ 1 TARAMA : kapanmış mumlar + canlı son bar → MarketInput
 → 18 strateji (panik-izole) → ensemble → rejim kapısı
 FAZ 2 TAHSİS : conviction + korelasyon + bütçe → Candidate
 FAZ 3 YÜRÜTME : durum makinesi Action'ları → WireOrder/Cancel
 → TEK batch POST (cloid'li)
 FAZ 4 ÇIKIŞLAR : borsada asılı SL/TP trigger'lar + trailing ratchet
 + makine ExitNow + çıkış kanalları
 FAZ 5 ÖĞRENME : kapanan işlem R → tracker → etkin ağırlıklar
```

### Execution graph (kanıtlanmış sözleşme)

```
strategy
 → ensemble (oylar → skor; Exit ayrı kanal: exit_requested)
 → decision (rejim kapısı + conviction + min_score)
 → order (IOC giriş, mark-bağlı fiyat bandı, cloid'li)
 → fill (WS orderUpdates + user fills; TP/SL oid eşleşmesi)
 → position state (Flat → Pending → InPosition → PendingExit → Flat)
 → exit reason (birebir kaynak eşleşmesi — bkz. mimari dokümanı)
```

---

## 2. Crate'ler

| Crate | Tek görev | Önemli API |
|---|---|---|
| `hyperliquid-api` | Borsa I/O (imza, emir, okuma, WS) | `Config`, `Exchange`, `Info`, `sign_l1_action`, `WsClient`, `ApiError` |
| `data` | WS→tick→mum + bağlam + kalıcılık + evren | `Ingestor`, `Aggregator`, `MarketCtx`, `Storage`, `TradingUniverse` |
| `math` | Saf sayısal çekirdek (f64, bağımlılıksız) | `indicators`, `linalg`, `portfolio`, `stats` |
| `strategy` | Sinyal sözleşmesi + 18 strateji | `Strategy`, `Signal`, `MarketInput`, `registry` |
| `engine` | Karar→emir→pozisyon→öğrenme | `Engine`, `Executor`, `AllocationManager`, `PositionStateMachine`, `RiskManager`, `StrategyTracker` |
| `utils` | Logger + NTP zaman | `logger::init`, `time::now_ms` |

### api

- **signing** — EIP-712 phantom-agent; `ExchangeOutcome`: SUCCESS / ERROR / **UNKNOWN** (ağ zaman aşımı: borsada işlenmiş OLABİLİR → yukarıya reconciliation için taşınır, asla yutulmaz).
- **exchange_endpoint** — tek/toplu emir, `OrderStatuses` (resting_oid / filled / error), fiyat 5-anlamlı-basamak, lot `szDecimals`.
- **info_endpoint** — market/hesap ham sorguları; `candle_snapshot` endTime ms ZORUNLU.
- **websocket** — 50 sn ping, 5 sn otomatik reconnect; alıcı düşerse görev sonlanır.
- **error_responses** — "emin olunamayan" hata KRİTİK sayılır (sessiz yutma yok).
- **config** — .env doğrulama (https/wss/32B-hex); hatalı konfig → süreci düşür.

### data

- **ingestor** — shard başına ~160 abonelik; evren DIŞARIDAN (`with_universe`) — sessiz kırpma yok.
- **aggregator** — çoklu interval mum; `is_stale` → sinyale karışmaz.
- **market_ctx** — `delta_oi` (likidasyon kadransı), funding/OI serileri, basis.
- **storage** — ClickHouse (üretim) / Memory (test+fallback) / Redis (sıcak cache); URL boşsa indirimli mod loglanır.
- **universe** — `UniverseSpec{All, Exact, TopByVolume}`; `verify_layers`: katman uyuşmazlığı → AÇILIŞ DURUR.
- **backfill_job** — sayfalı REST geçmiş pompası (5000 bar/istek).

---

## 3. Risk-kapı katmanları (savunma derinliği)

| Katman | Kapı | Otomatik açılır mı |
|---|---|---|
| 1. Strateji | veri yok → Skip; panik → Skip | — |
| 2. Rejim | SQUEEZE/VOL_SPIKE → yeni giriş yasak (çıkış serbest) | rejim değişince |
| 3. EV (CostModel) | `sl×ATR < max(EV_MIN_DIST, 3×rt)` → SKIP | — |
| 4. Tahsis | conviction < min → düşür; korelasyon > %70 → çakışan düşer | — |
| 5. Pre-trade | notional/kaldıraç/equity/drawdown tavanları | **HAYIR** — halt yalnız operatörle resume |
| 6. EntryGate | `entries_allowed = health_ok && !risk_locked` | health dönerse |
| 7. Risk-lock | flatten başarısız → kilit; **2 ardışık DOĞRULANMIŞ flat döngü** | otomatik (sync ile doğrulanırsa) |
| 8. Kill-switch | 5 ardışık karar-kritik hata → halt | **HAYIR** |

**EntryGate invariant **: `entries_allowed` bir **alan değildir**; `EntryGate { health_ok, risk_locked }` üzerinden türetilir. Hiçbir runtime yolu doğrudan yazamaz; `entries_allowed_alan_olarak_saklanmaz` testi kaynak taramasıyla korur. Auto-unlock yalnız gerçek `sync_positions_from` ile doğrulanmış flat döngülerde ilerler (`rest_sync_verified`).

---

## 4. Çıkış sözleşmesi (birebir)

| Gerçek kaynak | ExitReason | trade-exit kodu |
|---|---|---|
| SL trigger fill (oid eşleşmesi) | `StopLoss` | `stop` |
| TP trigger fill (oid eşleşmesi) | `TakeProfit` | `tp` |
| Trailing ratchet'li SL fill | `TrailingStop` | `trailing` |
| Makine ExitNow (skor tersi) | `SignalExit` | `signal` |
| Kill-switch / black-swan flatten | `EmergencyFlatten` | `emergency` |
| Manuel | `Manual` | `manual` |

**Kural**: `exit_reason_from_fill` — bayrak (oid eşleşmesi) yoksa reason ÜRETİLMEZ; **MFE pozitif diye TakeProfit yazılmaz**. `apply_exit_reason` = first-write-wins.

---

## 5. TP / SL / trailing

```
SL(LONG) = entry_eff − 1.5×ATR SL(SHORT) = entry_eff + 1.5×ATR
tp_dist = max( (tp_atr_mult/sl_atr_mult) × risk , 2 × risk )
TP(LONG) = entry_eff + tp_dist TP(SHORT) = entry_eff − tp_dist
```

- `entry_eff` = **gerçek dolum** (REST `entryPx`); IOC kayma düzeltmesi `EntryAttr::apply_real_fill` ile tek noktada (giriş + TP yeniden türetme).
- **TP artık gerçek kapanıştır **: giriş dolumunda reduce-only TP trigger (`tpsl="tp"`, cloid'li) borsaya asılır; fill → `TakeProfit`.
- Çift emir yasağı: `sl_orders`/`tp_orders` haritaları + `TS_GRACE_MS` grace + `restore_tp_orders` (recon'da borsadan ±%0.1 fiyat eşleşmesiyle oid geri yükleme — körlemesine duplicate yok).
- Trailing : mark tepe/dip ratchet; kural: **önce yeni stop, sonra eski iptal**; `TRAIL_ATR_K=1.5`, `TRAIL_MIN_STEP=0.25`.
- **Kontrat (G)**: Long `SL < entry < TP`, Short `TP < entry < SL`; TP ≥ 2R; negatif fiyat (dejenere ATR) → **emir yok** (`entry_contract_ok`, 192 kombinasyonlu matris testi).

---

## 6. Öğrenme hattı

```
fill (kapanış) → R = net PnL / (risk_px × size)
 → StrategyTracker (pencere 100)
 → effective_weights:
 n < min_n(10) → donmuş 1.0 (cold-start; öldürme yok)
 10 ≤ n < 30 → dinamik Kelly, clamp [0.1, 2.0]
 n ≥ full_n(30) → win < %35 → 0 (sustur)
 keşif: n < full_n → kuyruk (round-robin, döngüde en fazla 1)
 → ensemble.cfg.weights
```

- **Canlı ağırlık snapshot'ı** (`origin:"live"`): 300 sn'de bir ATOMİK yazım; açılışta doğrulanmış okuma. Kanıtsız tracker'a EZİLMEZ (`merge_effective_weights`: kanıt varsa tracker kazanır, yoksa snapshot korunur). Bozuk dosya → **açılış başarısız**.
- Tracker state: `config/track_state.json` (atomik; restart'ta silinmez).
- Trade-exit arşivi: `trade_exits.jsonl` (append-only; epoch+git).

---

## 7. TESTNET öğrenme dönemi

```
TESTNET_FRESH_START=1 (İLK çalıştırma; epoch dosyası yoksa)
 ├→ eski tracker state silinir → tüm stratejiler nötr w=1.0
 ├→ eski canlı ağırlık snapshot silinir (look-ahead'den temiz)
 ├→ eski trade_exits.jsonl ARŞİVLENİR (*.prev — kayıp yok, karışma yok)
 ├→ yeni LearningEpoch yazılır: epoch_id / environment=testnet /
 │ started_at / starting_trade_count=0 / completed_trade_count=0
 └→ tüm pozisyonlar kapatılır + açık emirler iptal edilir (temiz taban)

normal restart / ENV kaldırıldığında
 └→ HİÇBİR state silinmez: sayaç + öğrenme kaldığı yerden devam eder
```

- **İşlem sayımı**: 1 gerçek kapanmış pozisyon = 1 `completed_trade_count`
 (sinyal/emir/fill sayısı İŞLEM DEĞİLDİR; TP/SL/trailing/signal-exit/
 emergency hepsi sayılır). Duplicate/replay fill'ler tid dedup ile
 düşer → ikinci kez sayım yok. Sayaç `learning_epoch.json`'da ATOMİK
 kalıcı; `trade_exits.jsonl` ile birebir tutarlı (her kapanışta tek
 TradeExit + tek sayım).
- Log: `[TESTNET] completed trades: N/100` … `100/100 tamamlandı — LIVE
 geçişi için hazır (otomatik canlı işlem YOK)`.
- 100 sonrası: `status = ready_for_live` kalıcı yazılır; canlı emir
 **başlatılmaz** (operatör kararı).
- **Yapısal koruma**: testnet epoch `NETWORK != testnet` veya canlı API
 url ile BAŞLAMAZ (emir hiç atılmadan süreç durur); `TESTNET_FRESH_START`
 yalnız testnet endpoint'te geçerlidir.
- P0/P1 güvenlikleri aynen korunur: reconciliation, state recovery,
 SL/TP, risk-lock, UNKNOWN reconciliation, live-weight validation,
 tracker persistence, tid dedup.

## 8. Strateji durum raporu

`[STRAT]` (saatte bir) her stratejiyi sınıflandırır — sessiz ölü strateji yasak:

| Durum | Anlam | Kimler |
|---|---|---|
| `ACTIVE` | girdisi tam, yönlü sinyal üretebilir | 15 strateji |
| `AUXILIARY` | giriş stratejisi değil (boyut/kote/plan önerisi) | Kelly Sizing, Market Making, Optimal Execution |
| `DATA-MISSING` | girdisi bağlı değil → Skip; sahte veri yok | Vol Arbitraj (IV feed yok) |

**Dürüst sınırlar** (icat yapılmadı): Elliott SHORT kolu kodda yok (testle kanıtlı); Değer Yatırım temel verisi harici (proxy Hold); likidasyon feed'i yok (OI'dan çıkarımsal); MM emir yolu global IOC TIF ile çelişir (quote planı yalnız not); **"Wyckoff" isim iddiası sınırlıdır** — gerçek Wyckoff analizi (faz sayımı, spring/upthrust, schematics) DEĞİLDİR; model = efor/sonuç prensibinin basitleştirilmiş hâli (trend + hacim eforu + aralık konumu). İsim, öğrenme kanıtının sürekliliği için sabit tutulmuştur.

---

## 9. Test katmanları

| Katman | Ne kanıtlar | Sayı |
|---|---|---|
| Strateji unit (18/18) | Long/Short koşulu, missing-data Skip, güç aralığı | 65 |
| Future-leak (prefix-replay) | onay verisi olmadan sinyal yok; gelecek barlar geçmişi değiştirmez | 6 strateji |
| Yön-simetri | Long/Short simetri; ters sahte sinyal yok | 6 strateji |
| Execution contract | SL/TP/RR 192 kombinasyon; yaşam döngüsü; UNKNOWN-cloid | 3 modül |
| Maliyet modeli | EV↔yürütme tek model; yüksek spread/kayma → SKIP | 3 |
| Metrikler | gerçek fill verisi; kanıt yoksa None; "%99" yasağı | 2 |
| Risk invariant | EntryGate denklemi + kaynak taraması; risk-lock | 21 |

`cargo test --workspace`: engine 135 + api 5 + data 9 + strategy 65 = **214 passed / 0 failed** (+1 ignored doc-test). Kalite raporu üç katman ayrıdır: **COMPILES PASS · TESTS PASS · TRADING LOGIC VERIFIED (koşullu: sınırlar 'de)**.

---

## 10. ENV referansı

| ENV | Varsayılan | Anlam |
|---|---|---|
| `ENGINE_INTERVAL` | 3600 | mum aralığı (sn) |
| `ENGINE_CYCLE_SECS` | 300 | motor döngü periyodu (sn) |
| `ENGINE_COINS` | tüm evren | evren spec (isim listesi / tamsayı N / all) |
| `ENGINE_HIST_BARS` | 4400 | indikatör ısınma penceresi |
| `ENGINE_BUDGET_USD` | 300 | döngü bütçesi |
| `ENGINE_TOP_N` / `ENGINE_MIN_SCORE` | 3 / 0.30 | pozisyon tavanı / ensemble eşiği |
| `ENGINE_LEVERAGE` | 5 | hesap kaldıracı |
| `ENGINE_FLATTEN` | 0 | açılışta pozisyon devri (recovery) — düzleşme YOK; her restart pozisyonları korur |
| `ENGINE_EXPLORE` | 1 | keşif kuyruğu |
| `ENGINE_CRIT_THRESHOLD` | 5 | kill-switch eşiği |
| `TRAIL_ON / TRAIL_ATR_K / TRAIL_MIN_STEP` | 1 / 1.5 / 0.25 | trailing stop |
| `LEARN_MIN_N / LEARN_FULL_N` | 10 / 30 | öğrenme eşikleri |
| `TRACK_STATE_PATH` | config/track_state.json | tracker persistence |
| `ENSEMBLE_WEIGHTS_IN` | config/ensemble_weights_live.json | canlı ağırlık girişi (origin=live zorunlu) |
| `ENSEMBLE_LIVE_WEIGHTS_OUT` | aynı dosya | atomik dump hedefi |
| `ENGINE_TICK_CACHE` | config/tick_cache.json | öğrenilen tick hassasiyeti |
| `TRADE_EXITS_PATH` | config/trade_exits.jsonl | trade-exit arşivi |
| `TESTNET_FRESH_START` | (yok) | =1: İLK çalıştırmada temiz deney (sonraki restartlar state'i korur) |
| `LEARNING_EPOCH_PATH` | config/learning_epoch.json | öğrenme dönemi + kalıcı işlem sayacı |
| `EV_TAKER_FEE_BPS / EV_SLIPPAGE_BPS / EV_MIN_DIST_BPS` | 4.5 / 5.0 / 75 | TEK maliyet modeli |
| `CLICKHOUSE_URL / REDIS_URL` | (boş) | kalıcılık; boşsa Memory fallback |
| `RUST_LOG / LOG_LEVEL` | info | log seviyesi |

---

## 11. Operasyon

- Başlatma: `scripts/start_bot.sh` (pgrep-temelli güvenli restart; önceki log `<log>.prev`).
- Log: `/tmp/opencode/bot.log`.
- Fiyat probu: `probe_px <COIN>`; trailing analitiği: `trail_report`.
- CI: `.github/workflows/test.yml` — push/PR→master'da `cargo test --workspace`.

*Test sayıları: engine 157, api 5, data 9, strategy 65 = **236 PASS / 0 FAIL** (+1 ignored doc-test). Son güncelleme: 2026-10-06.*
