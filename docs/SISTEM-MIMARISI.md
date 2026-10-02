# HYPERLIQUID BOT — SİSTEM MİMARİSİ v2

> Rust workspace: 6 crate + 1 binary. Tüm katmanlar canlı mainnet verisiyle doğrulandı.
> Bu doküman HER DOSYANIN ne işe yaradığını, çalışma mantığını ve katmanlar arası sözleşmeyi anlatır.
> Kıyamet senaryosu denetimi için: docs/KIYAMET-AUDIT.md (ayrı, acımasız rapor).

---

## 1. GENEL GÖRÜNÜM

### 1.1 Crate grafiği (bağımlılık yönü aşağıdan yukarı)

```
utils   (infra: saat/log)          ← hiçbirine bağımlı değil
math    (saf hesap, f64)           ← hiçbirine bağımlı değil
api     (HL konuşması: REST+WS+imza)
data    (akış + candle + bağlam + depolama)   ← api kullanır
engine  (defter + motor + tahsis + risk)      ← api, data, math, strategy, utils
strategy(18 sinyal stratejisi + ensemble)     ← math, data, api(sadece tip), utils
bot     (engine/src/bin/bot.rs)               ← hepsini birleştiren ana binary
```

Sirküler bağımlılık yok. Strategy crate'i api'den sadece tip seviyesinde (Candle
şekillendirme dışında) yararlanır — motor smi yalnızca engine'de.

### 1.2 Uçtan uca veri/aksiyon akışı

```
Hyperliquid mainnet
  §WS 4 shard (ingestor) → (tick, anlık defter snapshot, ctx, allMids)
  §REST 60 sn tam tarama (drift düzeltme)
        ↓ StreamEvent (mpsc, 8192)
	ftty Aggregator   tick→candle (1m/5m/15m/1h/4h/1d) — taker/VWAP türevleri
	MarketCtx         funding/OI/mark serileri (ΔOI)
	ClickHouse        kalıcı arşiv (hl_trades/hl_candles/hl_ctx)
	Redis             sıcak önbellek (son mids/ctx, TTL)
        ↓ (candle kapanışı döngü adımını zorlar)
	STRATEGY (18)     MarketInput → Signal (Long/Short/Hold/Exit/Skip)
	ENSEMBLE          ağırlıklı tek karar + rejim kapısı
	ALLOCATION        conviction = (oy×kelly)/ATR; korelasyon>%70 ele;
                         conviction-temelli sermaye dilimleri (top-N)
	RISK KAPISI       drawdown / equity tabanı / toplam maruziyet / kaldıraç
	STATE MAKINESI    Flat→Pending→InPosition→PendingExit→Flat
                         (idempotent/redd-geri/short-partial/trailing)
	BATCH EXECUTOR    TÜM emirler BİR imzalı POST (rate-limit dostu)
        ↓
Hyperliquid mainnet
```

---

## 2. API CRATE — Hyperliquid konuşması (dosya dosya)

| Dosya | İçerik / mantık |
|---|---|
| `signing.rs` | İmza zinciri: action→msgpack→+nonce(BE)+vaultprefix→keccak=action_hash→EIP-712 phantom-agent (Exchange/1/1337/0x0, source `a`)→secp256k1 RFC6979, low-s, v=27+recid. `now_ms()` nonce üretici. `float_to_wire` fiyat/boyut gödsutersi. HTTP taşıma: `info()`/`exchange_action()` (zarf: action+nonce+signature+vaultAddress+expiresAfter). SDK 0.24 ile bayt-bayt doğrulanmış. |
| `info_endpoint.rs` | Okuma metotları: meta, metaAndAssetCtxs ([meta,ctxs] formatta, context = funding/OI/premium/mark/oracle/dayNtlVlm/prevDayPx), allMids(+spot @), l2Book(levels[0]=bids,[1]=asks), candleSnapshot (endTime gerçek ms ZORUNLU), fundingHistory, clearinghouseState (MASTER adres şart), userFees/openOrders/userFills/orderStatus, spotMeta, predictedFundings, `name_to_asset` (coin→asset ID). |
| `exchange_endpoint.rs` | Emir zarfı: `{"type":"order","orders":[wire],"grouping":"na"}`; wire={a,b,p,s,r,t}; TIF Alo/Ioc/Gtc. place_limit/place_ioc/place_trigger/cancel/cancel_by_cloid/modify(batchModify)/update_leverage/update_isolated_margin/schedule_cancel. `round_5sig`(5 anlamlı hane)/`round_sz_up`(szDecimals). **batch_place/batch_cancel**: çoklu emir/iptal BİR POST (batching). |
| `websocket.rs` | `WsClient::run(subs,tx)`: bağlan→abonelik aç→(channel,data) pompla; 50sn ping (sunucu 60sn kapar), kopınca 5sn'de reconnect. `WsEvent{channel,data}`. |
| `error_responses.rs` | ApiError: Http{status,body}/Api(HL mesajı)/Sign/Json/Hex/Ws/Other(anyhow). OrderStatuses.error(i) batch redlerini taşır. |
| `types.rs` | Wire şemaları: PerpMeta/Meta/AssetCtx(	prevDayPx dahil)/Candle(t,T,s,i,o,c,h,l,v,n)/L2Book/L4Level/FundingEntry. |
| `config.rs` | .env: NETWORK(mainnet), URL'ler, PRIVATE_KEY(agent imza), API_WALLET_ADDRESS, USER_ADDRESS(master sorgu), LOG_LEVEL, CH_REDIS URL'leri. Doğrular: pk 32B, https/wss. |

Anahtar ayrımı: **agent key = imza**, **master address = hesabın sorgusu** (agent
adresi sorgularda boş döner — resmi kural).

---

## 3. DATA CRATE — canlı veri (dosya dosya)

| Dosya | İçerik / mantık |
|---|---|
| `ingestor.rs` | `load_universe`: metaAndAssetCtxs TEK çağrı→delist/hacim eşiği filtreleme, hacme göre sıralı. `Ingestor::start`: coinler N shard WS'e bölünür (trades+activeAssetCtx+ilk-N l2Book; shard0 allMids). `translate`: WsEvent→StreamEvent; **trades dizisi tek tek** (tick kaybı 0). StreamEvent: Trade/Book/Mids/Ctx. |
| `aggregator.rs` | tick→candle bucket (ts div iv); kapanışta mum + history'ye (`history_len_max` × `history_span_secs/history_span_min_iv` — **sabit 500 değil**, indikatör ısınma penceresi kadar; varsayılan ~4400×1h). `taker_balance(5dk buy/sell dengesi)` + `vwap(oturum)` + `backfill`(REST geçmiş). Sunucu zamanı kullanır. |
| `market_ctx.rs` | `CtxState` (coin, funding, oi, mark, oracle, premium, dayVlm, **prevDayPx**, ts). WS Ctx + REST tam tarama birleşik. Türetilen: `delta_oi`(likidasyon kadransı), oi/funding serileri, `basis`(spot-perp). |
| `backfill_job.rs` | **Tarih pompala**: paginasyon (5000 bar/istek, 250 ms uyku) + RESUME (CH'son t'den devam). CandleSink trait → ClickHouseStorage impl. `pump_funding`. |
| `storage.rs` | Storage trait + üç impl: **Memory**(yedek/test), **ClickHouse**(hl_trades/hl_candles/hl_funding/hl_ctx/hl_strategy_performance; JSONEachRow toplu; TSV count; ReplacingMergeTree dedup; last_candle_ms resume), **Redis** (mids/ctx TTL'li sıcak cache). |
| `bin/collect.rs` | (Kullanıcı tarafından kaldırılmış gerek yok; yenibakın) collect mod: 3 görev — tarihi pompa + WS canlı + 60s REST tarama (hepsi CH'ye). |

---

## 4. ENGINE CRATE — motor (dosya dosya)

| Dosya | İçerik / mantık |
|---|---|
| `state_machine.rs` | Durumlar: Flat→Pending{side,oid,remaining}→InPosition{side,entry,size,peak}→PendingExit→Flat. Garantiler: **Idempotency** (aynı yönlü sinyal yeni emir üretmez), **Red→Flat**, **Kısmi dolum** (kalan Pending'te; timeout→Cancel), **Trailing stop** (peak izleme; %1.5 pull → reduce_only ExitNow), **State recovery** (borsadaki gerçek pozisyonlardan). 6 senaryo testi ✓ |
| `allocation_manager.rs` | İNANÇ = (oy yoğunluğu × kelly ağırlığı × güc) / (1+20·ATR%) ; SL/TP = mid±ATR kısımları; **korelasyon filtresi** >%70 elenme (açgözlü top-N) ; tahsis = conviction-temelli (fractional Kelly dilimleri). 4 test ✓ |
| `engine_loop.rs` | 3 Faz: 1) tarama (stratejiler→ensemble+sinyal kapısı+mumlar+ATR) 2) tahsis (Vote=coin×dir×oy×kelly+corr) 3) yürütme: machine.on_decision→Place/Exit/Cancel; Place yalnız seçilenlere (reddediyse pending→Flat revert). **Kuru koşum tamamen silindi** — her dolum `user` kanalından gelen gerçek borsa verisidir. **batch_execute: TEK POST** (max 2\*POST: place+cancel). |
| `executor.rs` | api Exchange ince katman: tick-lot doğrulama, cache'li asset çözümü (ağ çağrısız), open_orders defteri, pozisyon cache (assetPositions), WS orderUpdates/user işleme, refresh_orders/refresh_positions (recon). `batch_execute`: çoklu place+cancel→max 2 POST. |
| `orderbook.rs` | Yerel L2 defter: REST sync_from_rest + WS snapshot; mid/spread_bps/imbalance(5)/vwap_fill/depth_within_bps/is_stale/is_sane (crossed-book reddi). |
| `risk_manager.rs` | KAPISI: drawdown halt (`peak_equity`'den %20), equity tabanı (**$50**, `min_account_equity_usd`), coin başına $50 notional, toplam maruziyet, kaldıraç 5×, coin_emir sayısı. **`pre_trade()` YALNIZCA GİRİŞTE** çağrılır (Place kolu) — **çıkışlar hiçbir zaman bloke edilmez**, yani drawdown halt açıkken stop/exit hâlâ çalışır. `resume()` operatör kararı. |
| `track.rs` | **CANLI ÖĞRENME** (bkz. §8.4). Backtest kaldırıldı → 18 strateji **nötr w=1.0** ile başlar; attribution ile R kaydı; ROLLING 100 pencere; `n < LEARN_MIN_N`(10) → ağırlık **donmuş 1.0**; `10≤n<30` dinamik f\*=p−(1−p)/1.8 clamp [0.1,2.0]; `n ≥ LEARN_FULL_N`(30) → `win<%35` → **0**. **Persistence**: `TRACK_STATE_PATH` (atomik yazım, 5 dk). Kill-switch taksonomisi (`Health`) + `ExitsOnly` kipi burada. 20 test ✓ |

---

## 5. MATH CRATE — hesaplama (dosya dosya)

| Dosya | İçerik / mantık |
|---|---|
| `indicators.rs` | Konvansiyonlar: EMA seed=ilk; Wilder(RSI/ATR/±DI/ADX) seed=ilk-n SMA; pencere boşsa None. Göstergeler: SMA/EMA/RSI14/MACD(12,26,9)/Bollinger(20,2)/ATR14/ADX±DI/OBV/Stochastic/Donchian/Fib/rolling VWAP/TWAP. Vol: realized/EWMA(λ=0.94)/Parkinson/GarmanKlass/GARCH(1,1)+fit(varyans-hedefli koordinat inişi). Ann=365×24. |
| `portfolio.rs` | returns; Sharpe; Sortino(downside); maxDrawdown; Calmar; expectancy; Kelly binary f=p−(1−p)/b + sürekli μ/σ²; kovaryans/korelasyon matris (ddof=1); min-variance weights w=Σ⁻¹1/1'Σ⁻¹1; portfolio vol √(w'Σw). |
| `linalg.rs` | mat_mul/transpose/mat_vec; Gauss solve (pivotlu); inverse(Gauss-Jordan); Cholesky(SPD); OLS(beta/se/t/R²); linreg kapalı form. |
| `stats.rs` | z_score/zscore_last; **ADF** birim kök (MacKinnon %1/%5/%10 kritikleri; t<crit → durağan); ou_half_life; hurst (R/S; rejim filtresi). |

Doğrulama: numpy referans + canlı mumlar → **5.828 değer, maks sapma 5.9e-14** (float64 epsilon: sıfır).

---

## 6. STRATEGY CRATE — sinyaller (dosya dosya)

Trait: `Strategy{name(); evaluate(MarketInput)->Signal}`; `registry()` kayıt defteri.

| Dosya | Mantık özeti |
|---|---|
| `wyckoff.rs` | EMA20/50 trend + efor(hacim oranı)/sonuç(20bar konum) divergence; dry-up→kapatma; burst→yükseliş |
| `kesit_momentum.rs` | Evren **1g** getiri sıralaması (`mark_px/prev_day_px−1` — kodda 7g hesabı **YOK**, P1-G ölçüldü): üst %20 long / alt %20 short; sıralama kayması→Exit |
| `lob_emir_akisi.rs` | Defter dengesi(5 seviye) >+0.15 İLE taker>%55 ÇİFT onay; momentum kesişimi |
| `market_making.rs` | Avellaneda-Stoikov: r=mid−q·γ·σ²·T; quote planı ALO |
| `optimal_execution.rs` | U-şekilli parçalama planı; vol→Ioc/Alo seçimi |
| `volatilite_zaman_serisi.rs` | GARCH(1,1) fit→σ_next/σ_long<0.85 sıkışma→genişleme+momentum |
| `carry_funding_basis.rs` | funding·3·365>+%10→Long carry, <−%10→Short |
| `oi_likidasyon.rs` | ΔOI×ΔP 4 kadrans; kontra yönler %30 zayıf |
| `rejim_tespiti.rs` | ADX+Hurst+vol% → TREND/MR/SPIKE/SQUEEZE/GECIS; classify() motor kapısı |
| `risk_kelly.rs` | boyut katmanı: f*=1.8, %45 örneklem; canlıda tracker besler |
| `dow_teorisi.rs` | HH/HL pivot yapısı + hacim onayı + EMA50; eksikte eleme |
| `davranissal_finans.rs` | 2σ aşırı hareket + hacim 2× → panik/euphoria kontra fade |
| `klasik_indikatorler.rs` | RSI/MACD/BB/OBV/VWAP 5-oy skoru (|s|≥2 → sinyal) |
| `elliott_dalga.rs` | 5 dalga pivots + %30-65 Fib; güç zayıf [0.45] |
| `deger_yatirim.rs` | temel skor HARİCİ kanca; HL proxy: dayVlm/OI |
| `trend_following_turtle.rs` | Donchian 20 giriş / 55 filtre; birim=%1 equity/ATR |
| `breakout_kirilim.rs` | BB genişlik %35 sıkışma ön koşulu + 30-bar kırılım + taker/OI onay |
| `volatilite_arbitraj.rs` | VRP=IV−RV; IV yoksa RV terim yapısı proxy (10/100 bar) |
| `ensemble.rs` | Ağırlıklı birleşim + rejim kapısı + ağırlık dinamiği (yukarıda) |

---

## 7. UTILS — infra (dosya dosya)

| Dosya | Mantık |
|---|---|
| `time.rs` | SNTP v4: 4 sunucuya paralel, ofsetler MEDIAN (~1 ms); `now_ms()` düzeltilmiş saat; `rest_rtt`; measure. nonce (T−2g…T+1g) pencere geniş (2 gün) → 500 ms skewsiz zararsız; 30 dk resync bot döngüsünde. |
| `logger.rs` | tracing+env-filter (LOG_LEVEL>RUST_LOG>info; Once tek seferli). |

---

## 8. BINARY + AYARLAR

**engine/src/bin/bot.rs** — ana process:
1. logger+ NTP senkron
2. USER_ADDRESS (varsa) → WS orderUpdates/userEvents aboneliği → `engine.ingest_oe`
3. Ingestor WS (2-4 shard, top-30 coin) → `engine.ingest_step` (tokio::select!)
4. `ENGINE_CYCLE_SECS` döngü → `engine.run_cycle` → sinyal tablosu ekrana
5. 30 dk NTP resync; 5 dk ağırlık dump (`/root/hyperliquid/config/ensemble_weights.json`); 5 hata üst üste → durdur (kill-switch)

ENV: ENGINE_COINS, ENGINE_CYCLE_SECS, ENGINE_INTERVAL, ENGINE_HIST_BARS, ENGINE_TOP_N, ENGINE_MIN_SCORE, ENGINE_LEVERAGE, ENGINE_BUDGET_USD, ENGINE_FLATTEN, ENGINE_EXPLORE, ENSEMBLE_WEIGHTS_IN, ENSEMBLE_LIVE_WEIGHTS_OUT.

| ENV | varsayılan | anlamı |
|---|---|---|
| `ENGINE_INTERVAL` | 60 | **mum bar aralığı** (sn) — 1h strateji için 3600 |
| `ENGINE_CYCLE_SECS` | 5 | **döngü periyodu** (sn) — karar/uygulama sıklığı; 1h bar ile 300 |
| `ENGINE_HIST_BARS` | 500 | backfill mum sayısı — **1h ile 4400** (eğitimle aynı) |
| `ENGINE_TOP_N` | 3 | azami **açık** pozisyon (döngü seçimi + açık tavanı) |
| `ENGINE_MIN_SCORE` | 0.30 | ensemble kapısı |
| `ENGINE_LEVERAGE` | 5 | kaldıraç (coin'in `maxLeverage`'ine kırpılır) |
| `ENGINE_BUDGET_USD` | — | verilirse bakiye-oranı yerine mutlak bütçe |
| `ENGINE_FLATTEN` | — | `1` → açılışta tüm pozisyonları kapat, sıfırdan başla |
| `DRY_RUN` / `DRY_RUN_EQUITY_USD` | — | **YOK** (kuru koşum tamamen silindi) |

`ENGINE_INTERVAL` ile `ENGINE_CYCLE_SECS` **ayrı kavramlar**: ilkisi stratejilerin
bakacağı bar büyüklüğü, ikincisi motorun ne sıklıkla karar verdiği. 1h bar ile 5 sn
döngü motor boşuna 720× aynı sinyali yeniden hesaplardı.

Bot **gerçek emir yoluyla** çalışır: karar → tahsis → risk kapısı → kaldıraç → `batch_place` → `user`/`orderUpdates` dolum → pozisyon makinesi.

### 8.0.2 Risk kapıları: hangi cap, hangi birim

Birim belirsizliği uzun süredir belgelenmemişti. Ölçülen değerler:

| Cap | Değer | Birim | Nerede ölçülür |
|---|---|---|---|
| `max_position_notional_usd` | **$50** | **NOTIONAL** (kaldıraç çarpılmamış) | `pre_trade`: `proje = mevcut position_value + emir_notional > 50` |
| `max_total_notional_usd` | **$200** | NOTIONAL | `max_position_usd × (top_n+1)` = 50 × 4 |
| `min_account_equity_usd` | $50 | USD equity | `marginSummary.accountValue` |
| `max_leverage` | 5× | çarpan | coin'in `maxLeverage`'ine kırpılır |
| `max_drawdown_pct` | %20 | `peak_equity`'den düşüş | `resume()` gerektirir |
| `min_order_notional_usd` | $10 | NOTIONAL | borsanın alt sınırı |

**Worst-case hesap kaldıracı:** 3 pozisyon × $50 notional = **$150 notional**.
Equity $999'da → **0.15×** (mütevazı). Equity $100'de → 1.5×. Yani kaldıraç 5×
ayarı **bu botta fiilen kullanılmıyor**: risk kapağı notional'i $50 ile sınırladığı
için marj kullanımı zaten düşük. Kripto korelasyonu 1.0'a çıksa bile toplam maruziyet
$150'da kalıyor — "tek büyük bahis" riski **yok**.

Bu, tahsis ile riskin aynı tavanı paylaşmasının nedeni: `max_position_usd = $50`
notional, tahsis `$50`'ye kırpılıyor (§8.0.1).

### 8.0.1 Tahsis ↔ risk tavanı eşitliği

Tahsis (`AllocationManager::allocate`) ile risk kapısı (`RiskManager::pre_trade`)
**aynı** pozisyon tavanını kullanır (`AllocationConfig.max_position_usd`,
motor bunu `LoopConfig.max_position_usd`'den alır, varsayılan $50). Eşitlenmeden
önce bütçe tek güçlü adaya tamamen gidiyordu: bütçe $300, tek aday → `$300`
tahsis → risk `coin toplamı 299.99 USD > 50.00` ile reddediyor, sinyal boşa
gidiyordu. Artık `allocate` her adayı tavana kırpır ve kırpmayı loglar
(`[AL] ZK tahsisi $157 → $50`).

---

## 8.1 KEŞİF KUYRUĞU — veri açlığı (P0-C)

**Eski tanım yanlıştı.** Keşif = "ağırlığı ~0 olan strateji" idi; bu tanım
nötr cold-start'ta (§8.4) **anlamsızdır** — 18 stratejinin hepsi 1.0 ile
başlıyor, "ağırlığı 0 olan" diye bir şey yok.

**Yeni tanım: keşif hakkı = VERİ AÇLIĞI.**

| | Kural |
|---|---|
| Kuyruğa girme | `n < LEARN_FULL_N` (varsayılan 30) — yani **kanıtı yetersiz** her strateji |
| Sıralama | `n` artan (en az veri gören önce), sonra isim (deterministik) |
| Sıra dağıtımı | **round-robin**, döngüde en fazla **BİR** keşifçi (`explore_cursor`) |
| Zorunluluk | Sıradaki keşifçi **sinyal üretirse** `min_conviction` bypass + $10 taban; **üretmezse zorla işlem AÇILMAZ** |
| Çıkış | `n ≥ LEARN_FULL_N` → kuyruk bir daha görünmez (terfi/düşüş artık normal tracker akışında) |

Havuz matematiği **aynen korunur** (P0-C kapsamı dışında, zaten doğrulanmıştı):
`finanse_edilebilir = min(N, ⌊havuz/$10⌋)`, havuz = bütçenin ≤%5'i, taban = kişi başı
$10 (taban havuzu **aşamaz**). `explore_max_usd = $150` **ölü konfigürdür** —
havuz küçük hesapta $15 bile olamıyor; gerçek tavan havuzun kendisi.
Dokümante edilmiş kalıyor (silinmesi bütçe kararına bağlı).

Loglar:
```
[KEŞİF] sıra: Kesit Momentum (n=0 < full_n=30) | kuyruk=18 | sinyal veren=4 | sıradakiler=["Kesit Momentum","Wyckoff",…]
[AL] keşif tavanı dolu (1 kişi × $10 = $15.00) — ZK elendi
```
İlk satır **döngüde kim bypass aldığını**, ikincisi **havuzun ne kadar büyük
olduğunu** gösterir. Log örneğindeki `1 kişi × $10` = tavan payı; elenenler
kelimenin sonunda.

Terfi/düşüş artık keşif şeridinde değil, `track.rs` normal akışında:
`n ≥ 30` → f\* formülü; `win < %35` → ağırlık **0**.

---

## 8.2 BORSA EMİR KURALLARI (testnet'te ÖLÇÜLDÜ)

| Kural | Sonuç | Nerede |
|---|---|---|
| **Fiyat tick'i coin'e özel, meta'da YOK** | ME 6 haneyi REDDEDİYOR (0.073706→red, 0.07371→ok), AXS/BERA/MEME 6 haneyi KABUL, BTC 2 haneyi kabul | `round_px_for(px, max_decimals)` + `Executor::px_dec` cache + reddedince kaba ızgarayla yeniden deneme |
| **5 anlamlı hane** | `round_5sig` (fiyat önce 5 sig, **sonra** tick'e yuvarlanır; ters sırada ızgara bozulur) | `api/src/exchange_endpoint.rs` |
| **Post-only (ALO) giriş çalışmıyor** | long'da ask'a / short'ta bid'e yazmak spread'i kesiyor → "Post only order would have immediately matched" | giriş artık **IOC + mark tabanlı %0.5 kayma kapağı** |
| **reduce-only `sz=0.0`** | borsa "tüm pozisyonu kapat" diye yorumluyor (belirsiz!) | `state_machine` ters-sinyal çıkışı artık gerçek `size` taşıyor + `executor` hayalet koruması |
| **orderUpdates `sz`/`avgPx` boş gelebiliyor** | IOC tam dolumunda `sz=0.0` → makine yanlışa gidiyor | gerçek dolumlar `user` kanalından (`fill(user)`) besleniyor |
| `orderUpdates` payload **dizi** | `{"orders":[...]}` değil, doğrudan `[{order,status}]` | `apply_order_update` iki şekli de kabul ediyor |
| `clearinghouseState` ara sıra **`null`** | `positions.clear()` null'da borsadaki $400'lük pozisyonları "yok" sayardı | null = hata, eski harita korunuyor |

---

## 8.3 CANLI BUGFINDING → KOD DÜZELTMELERİ

Bunlar doküman okumasıyla değil, **çalışan botun logundan** bulundu; hepsi testnet'te ölçüldü.

### 8.3.1 Eğitim/live veri penceresi çelişkisi (`history_len = 500`)

`Aggregator.history_len` **sabit 500** idi. 1h bar ile canlıda 500 bar = ~21 gün,
eğitimde ise 4400 bar = ~183 gün kullanılıyordu; 8 gösterge (EMA200, VWAP, trend)
tamamen farklı ölçekte hesaplanıyordu. Ayrıca `backfill` **hiçbir kırpma
uygulamıyordu** (500 sınırı yalnız canlı tick yolunda idi).

Çözüm: `AggregatorConfig.history_span_secs / history_span_min_iv / history_len_max`
+ `cap_for(iv)` + `configure_history(span, max, min_iv)`; **hem** canlı tick yolunda
hem `backfill`'te uygulanır. Açılışta `[HIST]` loglanır:
`60sn:500 300sn:500 900sn:500 3600sn:4400 14400sn:1100 86400sn:500`.

### 8.3.2 Açık pozisyon tavanı yoktu (`TOP_N=3` → 30 pozisyon)

İki ayrı katman bozuktu:

1. `cap_positions` yalnız **bu döngüde seçilen** adayları sınırlıyordu; **zaten açık**
   pozisyonları saymıyordu. 5 sn döngüde bot her turda 3 yeni aday açıp listeyi
   sınırsız büyütüyordu (ölçüldü: 30 pozisyon, sonra 25).
2. Daha kötüsü: **`Executor::apply_fill` pozisyon haritasını hiç güncellemiyordu**
   ve `refresh_positions()` yalnız açılışta çağrılıyordu. WS dolumları
   `fills_q`'ya düşüyor, `positions` haritasına yazılmıyordu → motorun gördüğü
   harita **açılıştan beri boş** kaldı. Tavan ekledikten sonra hâlâ 3 açık
   pozisyon varken 4. giriş açıldı.

Çözüm:
- WS dolumu (`apply_fill`) haritayı yazar: `szi ±= sz`, kapanışta (`|szi| ≈ 0`)
  coini haritadan düşürür.
- `sync_positions_from()` her döngüde `clearinghouseState`'ten yeniden kurar —
  equity için **zaten çekilen** REST yanıtının `assetPositions` dizisi, ek istek
  maliyeti yok. WS bir olay kaçırırsa harita kendini düzeltir. Boş/çözümsüz
  dizi gelirse hata sayılır, eski harita korunur.
- `open_now = positions().filter(|sz| ≠0).count()`; `open_now >= cap_positions`
  ise yeni giriş elenir (+ log), **elde tutulan coine yeniden giriş** engellenir.

### 8.3.3 Borsa stop'u hiç kurulmuyordu — iki ayrı hata

a) **Yanlış WS kanalı.** Borsa dolumları `channel:"userEvents"` ile yayınlıyor,
`handle_ws_event` ise `"user"` ile eşleştiriyordu → **tüm dolumlar sessizce
düşüyordu**. Artık `"user" | "userEvents" | "userNonFundingLedgerUpdates"` kabul
ediliyor (`fill(user)` logları belirdi).

b) **`pending_sl` bir sonraki dolum döngüsünde temizleniyordu.** Ana döngü
`oe_rx`'i `run_cycle` beklerken okuyamıyor; dolum ancak `run_cycle` bittikten sonra
işlenir. "Pozisyonu yok → pending sil" kuralı, dolum gelmeden stop'u siliyordu
(BORSA-SL:0, `[SL]` logu yok). Artık `pending_sl: HashMap<String,(f64,u64)>` —
fiyat + `ts_ms`; temizlik **10 dk yaşlanma** ile yapılıyor.

Stop **giriş dolumuyla aynı anda** borsaya asılıyor (`apply_fill` → `ensure_sl`),
döngü beklenmiyor: `Engine::sync_exchange_sl()` her döngü sonunda ve
`flatten_positions()` içinde de çalışır.

`place_sl_grid()` stop'u 5 haneye yuvarlayıp "invalid price" dönmesi hâlinde
ondalık basamağında ızgara yürüyerek yeniden dener (ME 6 haneyi reddediyor).

**Uçtan uca doğrulandı:** ALGO stop'u borsada kuruldu (`oid=61631251772`) ve
gerçekten tetiklendi — `fill(user) coin=ALGO px=0.12977 closed=-0.35343`.

### 8.3.4 `flatten_all` kapatamıyordu

Kapatma emirleri `place_limit` ile gönderiliyordu ve o yol **uç fiyat keşfine**
(`px_dec`) sahip değildi; ME 4 farklı kapama fiyatında da "Order has invalid
price" döndü. Artık `flatten_all` `batch_execute` kullanıyor (genişleyen kapak
%1 → %3 → %6 → %9), doğrulama turu sonrası `[FLAT] hesap temiz`.

### 8.3.5 `place_trigger` tick yuvarlaması

`place_trigger` fiyatı tick'e yuvarlamıyordu (giriş/çıkış limit emirleri
kullanıyordu) → ME gibi 6 haneyi reddeden coinlerde borsa stop'u hiç kurulamazdı.
`round_px_wire(trigger_px)` eklendi.

### 8.3.6 Öğrenilen tick'ler restart'ta unutuluyordu

Coin'in tick hassasiyeti meta'da **yok**; `Executor.px_dec` "invalid price"
reddedince ızgara yürüyerek öğreniyor ve süreç sonuna kadar bellekte tutuyordu.
Her restart bu öğrenmeyi sıfırlıyordu (özellikle ME'de 4+ başarısız emir).

`save_px_dec` / `load_px_dec`: `config/tick_cache.json` (`ENGINE_TICK_CACHE`),
5 dk'lık dump döngüsünde yazılır, açılışta okunur
(`[TICK] N coin'in tick hassasiyeti diskten geri yüklendi`).

### 8.3.6b Stop tetiklenince kayıt düşmüyordu → stop iki kez kurulup iptal edildi

Borsa stop'u tetikleyip pozisyonu kapattığında `sl_orders[coin]` hâlâ oid'yi
tutuyordu. Sonraki döngüde `sync_exchange_sl` "pozisyon hâlâ açık" görüp aynı
stop'u **yeniden** kuruyor, sonraki döngüde de iptal ediyordu (ölçüldü:
oid=…545184 kuruldu → 3 dk sonra oid=…857641 tekrar kuruldu → o da iptal).

İki kapı eklendi:
- `apply_fill`: dolumun oid'i `sl_orders[coin]` ile aynıysa stop **kendi
  tetiklenmiş** demektir → kayıt düşer
  (`[SL] ZK borsa stop TETİKLENDİ (oid=…)`).
- `sync_positions_from`: borsada kapanmış coin'in `sl_orders` + `pending_sl`
  kayıtları da düşer (stop "hâlâ emirdir" deyip yeniden kullanılmasın).

### 8.3.6c `already_held` kontrolü ölüydü

`pmap` state machine'lerden kuruluyor ve yalnız `position_value` taşıyor;
`szi` `Default::default()` = 0.0. `already_held` `pmap`'e bakıyordu → **her
zaman false**. Ölçüldü: ZK açıkken aynı coine ikinci kez (üstelik ters yönde)
giriş yapıldı, pozisyon ikiye katlandı. Artık `executor.positions()` (borsanın
kendi verisi) okunuyor.

### 8.3.6d WS reconnect `sl_orders`'ı siliyordu → ikinci stop

Testnet WS `502 Bad Gateway` / `connection reset` ile düşüyor, motor yeniden
bağlanıyor. `sl_orders` **yalnız bellekte** tutulduğu için reconnect'te kayboluyor
ve `sync_exchange_sl` aynı stop'u **ikinci kez** kuruyordu. İki reduce-only stop
birden pozisyonu kapatmaya çalışıyor, borsa birini `reduceOnlyCanceled` ile
iptal ediyordu (ölçüldü: oid=…86187 ve oid=…07357 aynı BIGTIME pozisyonunda).

Üç kapı:
- `refresh_orders()` artık `isTrigger && reduceOnly` emirleri `sl_orders`'a geri
  yüklüyor (yalnız hâlâ açık pozisyonu olan coinler için).
- `sync_exchange_sl` eksik stop sayısını görünce `refresh_orders()` çağırıyor
  (10 döngüde bir beklemiyor).
- S5 yetim-emir watchdog'u `sl_orders`'taki oid'leri **yetim saymıyor**
  (botun kendi giriş/çıkış emirlerini `machines` tutar; stop'lar orada yok).

`websocket.rs` artık reconnect sayacı loglar (`reconnect #N`) — testnet'te
dakikada birkaç kez oluyor, teşhis için gerekli.

### 8.3.7 Kill-switch yalnız duruyordu

Kill-switch (5 üst üste hata) motoru `break 'restart` ile durduruyordu; **açık
pozisyonlar borsada kalıyordu**. Artık önce `engine.flatten_positions()`.

### 8.3.8 `signing::post` retry'i çift emir üretiyordu (cloid yokluğu)

`post()` 429/5xx'te isteği 5 kez yeniden gönderiyor. 5xx'te **istek borsada
işlenmiş olabilir**, yanıt kaybolmuştur; retry aynı emri ikinci kez yollar →
pozisyon ikiye katlanır. Bu, S5'in çözdüğü "borsada emir var, bizde yok"
senaryosunun ikizi, üstelik retry yüzünden bilinçli üretiliyor.

Çözüm: **`cloid` (client order id)**. Hyperliquid aynı `c` ile ikinci emri
reddeder → idempotency bedava. Şema: emir nesnesinin **içinde** `"c"`, değer
`0x` + **32 hex hane** (128 bit).

> İlk deneme **16 hane** üretti ve testnet tüm emirleri 422 ile reddetti
> (`HTTP 422: Failed to deserialize the JSON body`) — ölçüldü 09:22, 09:27'de
> düzeltildi ve `[FLAT] hesap temiz` doğrulandı. Yani bu kapı testnet'te
> fiilen kanıtlandı: cloid formatı yanlışsa **hiçbir emir geçmiyor**.

`make_cloid(coin, px, sz, salt)` = FNV-1a 128, **deterministik**: aynı girdi →
aynı anahtar. Tüm emir yolları (`batch_place`, `place_trigger`, `flatten`,
exit) cloid gönderiyor.

### 8.3.9 Stop 5 dakikada bir ikinci kez kuruluyordu (süre kapısı)

`ensure_sl` idempotency'si yalnız `sl_orders`'a bakıyordu. Ölçüldü: ALT /
BIGTIME / TURBO stop'ları 08:48'de kuruldu (`oid=…3197/3305/3304`), **5 dakika
sonra ikinci bir oid ile tekrar kuruldu** (`oid=…3980/4750/…`). Sebep: stop'un
`orderUpdates` "open" bildirimi geç geliyor ve `sl_orders` bu arada boş
kalabiliyor.

Çözüm: `sl_armed_at` (coin → stop'un kurulduğu an) + `TS_GRACE_MS = 600_000`.
Süre dolmadan aynı coin için yeni stop denenmez. `sl_armed_at`; `cancel_sl`,
`apply_fill`, `apply_one_order_update` ve `sync_positions_from` içinde düşer.

### 8.3.9b Çift stop'un **üç** kaynağı (hepsi ölçüldü, üçü de düzeltildi)

09:43'te kurulan stop'lar (oid=…1867/1870/1871) **09:48'de ikinci bir oid ile**
kuruldu (…165356/166048/166722). Ham `openOrders` yanıtı alındığında üç ayrı
hata zinciri görüldü:

| # | Hata | Kanıt |
|---|---|---|
| 1 | **`refresh_orders` stop'ları tanımıyordu.** `isTrigger`/`triggerPx` alanlarına bakıyordu; testnet bu alanları **göndermiyor** | ham yanıt: `{'coin':'GMT','side':'B','reduceOnly':True,'isTrigger':None,'triggerPx':None,'tif':None}` |
| 2 | **`tokio::spawn` `&mut self` yakalayamaz** → on-fill yolunda kurulan oid `sl_orders`'a **hiç yazılmıyor**, sadece loga düşüyor | `sl_armed_at` de yazılmadığı için `TS_GRACE_MS` kapısı da tutmuyor |
| 3 | Çift stop oluşunca borsa birini `reduceOnlyCanceled` ile iptal ediyor, `sl_orders` hangisinin ayakta kaldığını bilmiyor | bir sonraki taramada **üçüncü** stop ekleniyor |

Çözümler:
- **Stop tespiti**: artık "duran + `reduceOnly` emir stop'tur". Gerekçe: bot'un
  tüm diğer reduce-only emirleri **IOC**'dur (`flatten_all`, `Action::ExitNow`),
  IOC hiç dinlenmez → `openOrders`'a girmez; giriş emirleri `reduce_only: false`.
  Yani defterde kalan tek reduce-only sınıf stop'tur.
- **Kayıt**: spawn **öncesi** `sl_armed_at` iyimser yazılır (aynı döngüdeki
  `ensure_sl`'ı grace'e takar), oid `sl_tx` kanalıyla döner, `run_cycle` başında
  `drain_sl_results()` `sl_orders`'a yazar.
- **Öz-temizlik**: `dedupe_sl` aynı coin'de birden fazla stop bulursa **en yeni
  oid'i bırakır, eskilerini iptal eder** (oid zamanla artan sayıdır). Açılışta,
  10 döngüde bir ve `sl_orders` eksik her tarandığında çalışır.

### 8.3.15 Trailing stop — **SL ratchet, TP SABİT**

Karar ve gerekçe:

| | Karar | Neden |
|---|---|---|
| **SL** | ✅ **ratchet** (tepe ∓ k×ATR) | Zararı sınırlar. Trailing'in tek işi bu. |
| **TP** | ❌ **sabit** (`tp_atr_mult = 3.0`) | Somut sayı (giriş 100, ATR 2, risk 3): sabit TP 106 → ödül:risk **2:1**. Fiyat 106'ya dokunup geri dönerse trailing TP 103'e çekilir → **1:1**. Yani trailing TP, tam TERSİR (kâr satışında) oranı yarıya düşürür, kazanan işlemleri kaybettirir. |

> **Doküman düzeltmesi:** bu bölüm ve KIYAMET-AUDIT "3:1" diyordu.
> `sl_atr_mult = 1.5` / `tp_atr_mult = 3.0` → gerçek oran **2:1**. Test
> `tp_sabit_kalir_trailing_yok` gerçek sabitlerle yazıldı.
>
> **TP NEREDE? TP yalnız bot-tarafıdır; borsada resting TP asla konmaz.**
> Borsada yalnız **stop** vardır (`place_trigger`, reduceOnly, triggerPx).
> TP tetiklendiğinde bot `ExitNow` (reduce_only IOC) gönderir. Nedeni:
> yerleştirilen her şey bir kırılma noktasıdır — iki borsa emri aynı pozisyonu
> iki kez kapatmaya çalışırsa biri "position does not exist" ile reddedilir,
> diğeri bekler; daha kötüsü, TP borsada dururken bot'un trailing SL'si
> hareket edemez (koruma katmanı iki yönlü çelişir). Tek doğruluk kaynağı
> **bot içi mantık**; borsa katmanı yalnız felaket-stop'u.

#### Uygulama

`sync_exchange_sl` içinde, her döngüde açık pozisyonlar için:

1. **Mark ile tepe/dip güncelle.** Bar yüksek/almak **kullanılmıyor**: 1 saatlik
   barda tepe, stop'u borsada olduğu anda tetikleyecek bir seviyeye taşıyabilir.
   Mark daha gerçekçi ve daha muhafazakâr.
2. **Hedef** = `tepe ∓ k×ATR`, `k = TRAIL_ATR_K` (varsayılan 1.5, giriş stop'u
   ile **aynı** → ilk adım nötr, stop gevşemez).
3. **Gevşeme kapısı:** long için hedef mevcut stop'un üstünde, short için altında
   olmalı. Tersi **asla** uygulanmaz.
4. **Min adım:** iyileştirme `TRAIL_MIN_STEP` (0.25) × ATR'den azsa emir
   yakılmaz — her tetikleme 1 `place_trigger` + 1 `cancel` = 2 imzalı POST.
5. **Doğru sıra: önce yeni stop, sonra eskiyi iptal** (`Executor::replace_sl`).
   Ters sırada (`iptal → yerleştir`) yerleştirme başarısız olursa pozisyon
   **sıfır korumasız** kalır — ölçüldü: `place_trigger` fiyat ızgarası
   bulamayınca ("invalid price") tüm denemelerde başarısız (ME: 4 deneme).
   Doğru sırada **en kötü senaryo "trailing gerçekleşmedi", "korumasız pozisyon"
   değildir.**

`TS_GRACE_MS` grace kapısı `replace_sl`'de **bilinçli olarak atlanır**: bu ikinci
bir stop değil, aynı stop'un yeni seviyeye taşınmasıdır.

#### MFE/MAE — kararı kanıtlamak için

`[MFE] coin LONG MFE=+3.33R MAE=-1.00R süre=812sn ratchet=2 son_mark=…`
Kapanan her pozisyonda tepe/dip R cinsinden loglanır. **Soru:** "stop biraz daha
yukarı çekilseydi bu işlem kârlı kapanır mıydı?"

Bu log olmadan "trailing işe yarıyor" ölçümsüz iddiadır. Mevcut 4 kayıp
örneğin hepsi 1-4 dk içinde stop'landı (MFE ≈ 0) — yani mevcut veri
trailing'in **işe yaramayacağını** söylüyor. Bir sonraki karar (trailing'i
tut / kapat / k parametresini değiştir) bu veriye göre verilecek.

#### Yazarken çıkan iki tuzak (ikisi de canlıda ölçüldü)

**1. `ensure_sl` idempotent dönüşü trailing'i sıfırlıyordu.**
`ensure_sl` hem "yeni stop kurdum" hem "zaten var" durumlarında `Ok(())`
döndürüyordu. Çağıran taraf, trailing'in yeni taşıdığı seviyeyi bir sonraki
satırda **sabit giriş stop'una geri yazıyordu** → her döngü kendini sıfırlıyor,
hiç ratchet olmuyor (ölçüldü: ilk canlı koşuda 0 ratchet). `ensure_sl` artık
`Result<bool>` döndürüyor (`true` = yeni kuruldu) ve `cur_sl_px` **yalnız** o
durumda güncelleniyor.

**2. REST `limitPx` stop fiyatı DEĞİL.** `place_trigger(is_market = true)`
gövdeyi `trigger × 1.05` (alış stop) / `trigger × 0.95` (satış stop) yolluyor;
REST `openOrders` yalnız `limitPx` döndürdüğü için **stop fiyatı tersine
hesaplanmadan** okunamıyor. Ölçülen canlı eşleşme:

```
LAYER stop kuruldu px=0.07868414  →  REST limitPx=0.082614  →  /1.05 = 0.078680 ✓
TURBO stop kuruldu px=0.00106343  →  REST limitPx=0.001116  →  /1.05 = 0.00106286 ✓
```

Ters çevirme yapılmazsa restart sonrası stop seviyesi **%5 yanlış** okunur → yanlış
ratchet kararı, ya da mevcut stop'un **gevşetilmesi** (korumanın sessizce
zayıflaması). Yön ters karıştırılırsa hata **%10.5** olur ve stop ters yöne
kaydırılır — yani kâr-garanti eden emir. İki test eklendi
(`stop_px_tests`). `OrderInfo` bu yüzden `asset` alanını taşıyor (tick ızgarası).

#### Gözlemlenebilirlik

`[TRAIL] coin SHORT mark=… tepe=… (entry=…) ATR=… stop=… hedef=… ratchet=N`
her 6 döngüde bir. Bu satır olmadan "neden ratchet olmadı" ayırt edilemiyordu.

#### Yapılandırma

| Değer | Varsayılan | Ortam |
|---|---|---|
| açık/kapalı | açık | `TRAIL_ON=0` kapatır |
| k (stop mesafesi / ATR) | 1.5 | `TRAIL_ATR_K` |
| min adım (ATR kesri) | 0.25 | `TRAIL_MIN_STEP` |

Bot içi `state_machine.trailing_exit_pct` (%1.5 pull → IOC çıkış) **ayrı ve
aynı anda** çalışır: IOC ile piyasadan çıkar (hızlı), borsa stop'u güvenlik
ağıdır. İkisi çelişmez; hangisinin önce tetikleneceği fiyat hareketine bağlı.

### 8.3.16 Stop **dolum fiyatından** hesaplanmalı — tüm kayıpların kök nedeni

**Ölçülen olay (canlı, 12:12):**

```
12:12:35  emir gönderiliyor GMT px=0.009035 buy=false   (IOC, mark×0.995)
12:12:37  fill(user) GMT px=0.009083                     ← GERÇEK dolum
12:12:38  [SL] stop kuruldu px=0.00900146                 ← dolumun %0.9 ALTINDA
12:12:39  fill(user) oid=stop px=0.00912 closed=-0.2072   ← 1.1 saniyede tetiklendi
```

**Neden.** `pending_sl` **mutlak stop fiyatı** taşıyordu ve o fiyat karar
anındaki **defter** fiyatından (`e.bid`) hesaplanıyordu. IOC kayması dolumu
%0.9 daha kötü getirdi → kısa pozisyonun stop'u dolumun altında kaldı → zaten
piyasada olduğu için anında tetiklendi.

Bu, "stop yerinde duruyor ama kâr kaybediyor" değil, **"stop işlemin kendisini
kapatıyor"** demektir. Önceki 4 kayıp örneğin ve "hepsi 1-4 dk içinde
stop'landı" gözleminin kök nedeni büyük olasılıkla budur.

**Düzeltme — mesafe taşınır, fiyat hesaplanır:**

| | Önce (hatalı) | Sonra |
|---|---|---|
| `pending_sl` | `(mutlak_sl_px, ts)` | `(risk mesafesi, ts)` |
| Stop fiyatı | karar anı defter px ± risk | **dolum px ± risk** |
| Kayma etkisi | stop'u yan tarafa taşır | **etkisiz** (stop dolumun lehli tarafında kalır) |

Ek olarak **sert yan kapısı**: stop dolum fiyatının hatalı tarafına asla
konulamaz; minimum mesafe %0.25. `risk` bozuksa (negatif/NaN/MAX) kapı devreye
girip stop'u doğru tarafta tutar.

Aynı kaynak hatadan `entry_attr.entry_px` de etkileniyordu (defter fiyatı
dolumdan farklı) → stop'un onarım yolu (`ensure_sl`) ve R-çarpanı/MFE-MAE de
aynı yanlış değeri kullanıyordu. Artık her döngüde REST `entryPx` ile
düzeltiliyor:

```
[SL] GMT giriş fiyatı düzeltildi: karar(defter)=0.0090015 dolum(REST)=0.009083 kayma=+0.91%
```

Kayma her zaman olur (IOC + ince defter); önemli olan stop'un ondan
**etkilenmemesi**. Ölçülen kayma %0.9, minimum stop mesafesi %0.25 → bu ikisi
çakıştığında eski kod her seferinde kendi stop'unu tetikliyordu.

### 8.3.10 `entry_attr` 24 saat sonra düşüyordu → **sessiz ters seçim bias'ı**

`entry_attr` (voter'lar, giriş fiyatı, risk mesafesi, boyut) zaman filtresiyle
24 saatte bir siliniyordu. İki sessiz bozulma:

1. **Risk profili değişiyordu.** `sync_exchange_sl` SL fiyatını
   `entry_attr.risk_px`'ten alıyor; kayıt yoksa %2'lik çok daha gevşek kurtarma
   stop'u kuruluyor. 24 saati aşan swing pozisyonunda koruma seviyesi sessizce
   düşüyordu.
2. **Terfi matematiği ters yönde bozuluyordu.** Pozisyon kapanınca R-çarpanı
   hesaplanamıyor → **kazanan ve uzun süren işlemler** PF tabanlı terfi
   matematiğinden düşüyor, sadece hızlı kaybedenler kalıyordu. Yani sistem
   sistematik olarak **kaybedenleri öğrenmeye** itiliyordu.

Düzeltme:
- `sync_positions_from` artık **kapanan coinleri döndürüyor**; kapanan coin için
  `entry_attr` düşer ve R-çarpanı `closed_pnl` üzerinden **bir kez** yazılır
  (`Executor::closed_pnl` + `take_closed_pnl`).
- Zaman filtresi yalnız **pozisyonsuz** artıkları toparlar (restart kalıntısı,
  emri hiç dolmayan giriş). Hâlâ tutulan pozisyonun kaydı korunur.
- WS dolumu kaçtıysa R **atlanır** ve uyarı loglanır — yanlış R yazmak,
  R'yi hiç yazmamaktan kötüdür.

### 8.3.11 "Boş dizi = hata" ayrımı yanlıştı → kalıcı kilitlenme

`sync_positions_from` önce `assetPositions` boş gelirse "hata" sayıp eski haritayı
koruyordu. Doğru ayrım:

| REST yanıtı | Anlam | Davranış |
|---|---|---|
| `assetPositions` **yok / null / parse edilemedi** | bozuk yanıt | eski harita korunur (`bail!` / `as_array()==None`) |
| anahtar var, dizi **boş** | hesap **gerçekten flat** | harita boşaltılır |

İkinci satır yazılmadığında: tüm pozisyonlar gerçekten kapandığında (stop'lar
borsada tetiklendi, `user` dolumu WS kopması yüzünden kaçtı) harita **ömür boyu
donuyordu** → `open_now` tavanı hiç açılmıyor, tahsis hep kapalı, P&L yanlış.
Testnet koşuları hep pozisyonlu olduğu için görülmemişti.

Kapanan coinler döndürülüyor; stop kayıtları (`sl_orders`, `sl_armed_at`,
`pending_sl`) de onlarla birlikte düşüyor. `Executor::parse_positions` saf
fonksiyona ayrıldı → 4 senaryo testi (`senaryo_tests`) bu dalı testnet
koşusuna gerek kalmadan doğruluyor.

### 8.3.12 `sync_exchange_sl` veri kaynağı: melez, bedava-ölçü

Doküman önceden belirsiz bırakıyordu. Net durum:

| Adım | Kaynak | Ekstra REST? |
|---|---|---|
| stop var mı kararı | `sl_orders` (bellek, WS ile beslenir) | hayır |
| `sl_orders` eksikse | `refresh_orders()` → `openOrders` REST | **evet, koşullu** |
| SL fiyatı | `entry_attr.risk_px`, yoksa girişten %2 | hayır |
| tetik doğrulama | WS `orderUpdates` (`triggered`) | hayır |

Yani döngü başına **her zaman** REST yok; sadece `sl_orders` açık pozisyon
sayısından azsa bir tarama yapılıyor. Testnet'in WS'i sık düştüğü için bu koşul
gerçekten sık tetikleniyor — bu durumda ek bir `openOrders` çağrısı oluyor
(S1'in tek-task darboğazının gerçek maliyeti, ölçülüp yazılmalı).

### 8.3.13 Kesit momentum evreni — **bilinçli karar**

İki kez "evren uyuşmazlığı" olarak raporlandı. Ölçüm: `universe_returns`
`ctx.all()`'dan kuruluyor, yani **158 coin'in tamamı** (`l2book_top_n` yalnız L2
defter derinliğini sınırlıyor, sıralamayı değil). `kesit_momentum` kendi içinde
`universe_returns.len() < 20` ise atlar, üst %20 / alt %20 long/short yapar.
**Pencere 1 GÜNDÜR** (P1-G ölçüldü): `ctx.mark_px / ctx.prev_day_px − 1`
(`activeAssetCtx`). Kod tabanında 7 günlük getiri hesabı **yoktur**; eski
doküman "7g sıralama" diyordu ve **kodla çelişiyordu**.

**Karar: değiştirme.** Backtest kaldırıldığı için "eğitim evreni" diye bir şey
kalmadı — **tek evren = canlı evren**, dolayısıyla eğitim/live çelişkisi yapısal
olarak imkânsız. Tek giderimi: defteri olmayan coin'lerin oyu
toplamdan düşüyor (`n_skip_nobook` loglanır) — bu **zaten ölçülen** bir gerçek,
sinyalin ağırlığını değil sıralamasını etkiler.

### 8.3.14 Trailing tepe (`peak`) restart'ta kaybolur

`state_machine` recovery borsadaki gerçek pozisyonu (giriş, boyut) geri yükler,
**tepeyi (`peak`) geri yükleyemez** — tepe borsada tutulmaz. Restart sonrası
bot-side trailing exit (`trailing_exit_pct` = %1.5) ilk döngüde tepeyi `entry_px`
olarak alır, dolayısıyla pull hesabı restart öncesinden **daha geniş** olur.

Bu **bilinçli kabul**: etkisi yalnız restart anında bir çıkışın biraz daha erken
tetiklenmesi yönünde (risk azaltıcı, kâr düşürücü değil). Ayrı bir dosyada
`peak` tutmak, tick çakışmasına açık. Borsa stop'u §8.3.3 ile bağımsız ve
restart'a dayanıklı olduğu için asıl koruma zaten orada.

---

## 8.4 CANLI EĞİTİM — nötr cold-start (P0-B/P0-C/P0-D)

**Backtest motoru kaldırıldı** (`backtest.rs`, `backtest_multi.rs`,
`bin/backtest_train.rs` — 819 satır). Neden: o ağırlıklar **look-ahead biased**
bir motordan geliyordu (S19) ve kaynağı düzeltmek yerine **kaldırmak** daha
sağlam. Testnet bedava ve gerçek dolum akışı veriyor.

### 8.4.1 Önyükleme

| Durum | Sonuç |
|---|---|
| `ensemble_weights_live.json` yok | `[LEARN] nötr başlangıç: 18 strateji × w=1.0; persistence=ON (min_n=10 full_n=30)` |
| `origin != "live"` (örn. eski backtest çıktısı) | **REDDEDİLİR** + WARN → nötr cold-start |
| `origin == "live"` | ağırlıklar yüklenir, eksik stratejiler 1.0'a düşer |

Varsayılan yol **`ensemble_weights_live.json`** (eski `ensemble_weights.json`
artık **anlamsız**: backtest çıktısı ve biaslıydı).

### 8.4.2 Ağırlık politikası (tek tablo)

| Pencere `n` | Ağırlık |
|---|---|
| `n < LEARN_MIN_N` (**10**) | **1.0 sabit — DONUZ.** 2-3 rastgele işlem ağırlığı öldürmesin |
| `10 ≤ n < LEARN_FULL_N` (30) | dinamik `f* = p − (1−p)/1.8`, clamp **[0.1, 2.0]** |
| `n ≥ LEARN_FULL_N` (30) | tam kurallar: `win < %35` → **0** |

ENV: `LEARN_MIN_N`, `LEARN_FULL_N`.

### 8.4.3 Persistence (S20 #4 kapandı)

Tracker her **5 dakikada** `TRACK_STATE_PATH` (varsayılan
`config/track_state.json`) dosyasına **atomik** (tmp + rename) yazılır:
her strateji için `n`, `win`, `pf`, `ΣR`, `last_ts`. Açılışta yüklenir.

Yazılmazsa restart'ta tüm canlı öğrenme kaybolur ve bot **her seferinde**
18×1.0'a düşer → hiç öğrenemez. `entry_attr` (pozisyon başına R girdisi)
RAM'de kalır; restart kabul edilebilir (yalnız o pozisyonun kapanışında R
kaydı düşer).

### 8.4.4 Gözlemlenebilirlik

Açılışta **tek satır** cold-start, sonra **60 saniyede bir** tablo
(5 sn döngüde her tur basmak log spam olurdu):

```
[LEARN] 18 strateji | kuyruk=18 | cursor=7
Kesit Momentum          w=1.00 n=0    win=-    pf=-    ΣR=+0.00
Dow Teorisi             w=0.42 n=12   win=42%  pf=1.10  ΣR=-0.80
Elliott Dalga           w=0.00 n=34   win=29%  pf=0.71  ΣR=-4.20
```

Ağırlık **değiştiğinde** ayrıca `[LEARN] TERFİ/SUSTUR: … 1.00→0.42 (n=12 …)`
satırı düşer.

### 8.4.5 Fee-farkındalıklı EV kapısı (P0-D) — SKIP, clamp değil

Sayılarla (hepsi ölçüldü):

```
round-trip maliyet ≈ 2 × (taker 4.5 + slippage 5.0) = 19 bps   (%0.19)
stop mesafesi = 1.5 × ATR  →  düşük volatilitede taban %0.25 (25 bps)
R = 0.25 / 0.19 ≈ 1.3     →  TP = 2×SL = %0.50  →  başabaş WR ≈ %59
```

`WR = %59`'a kadar **yapısal olarak negatif EV**. Testnet kayıpları
(R ≈ $0.125) birebir bunun sonucu.

**Kural:** `min_risk_dist_bps = max(75, 3 × round_trip_cost_bps)`.
Tahsisten sonra, yürütmeden önce: `1.5×ATR_bps < min_risk_dist_bps` →
`Signal::Skip`. Defter inceyse floor'a `spread_bps × 3` de eklenir.

```
[EV] BIGTIME ATR 167bps → risk 250bps < maliyet flooru 750bps → SKIP
     (round-trip 190bps; işlem sayısının düşmesi BEKLENEN davranış)
```

> **İşlem sayısının düşmesi hata değil, DOĞRU davranıştır.** Düşük volatilite
> = yüksek likidite + dar spread = aslında en iyi coinler; ama `1.5×ATR` orada
> %0.25'e düşüyor. Doğru yanıt pozisyon almamak.
> ENV: `EV_TAKER_FEE_BPS`, `EV_SLIPPAGE_BPS`, `EV_MIN_DIST_BPS`.

### 8.4.6 Kill-switch taksonomisi + `ExitsOnly` (P1-F)

Eski kural "**5** hata üst üste → flatten + dur" idi; bu **çok geniş** bir
sayaçtı. Testnet WS'i dakikada birkaç kez düşüyor (`502 Bad Gateway`,
`Connection reset`) — yani **normal** olan durum sistemi öldürüyordu.

| Hata türü | Sayılır mı | Gerekçe |
|---|---|---|
| `clearinghouseState` art arda parse-fail | ✅ | pozisyon durumu **bilinmiyor** → risk hesabı yanlış |
| `batch_place` sonucu bilinmiyor | ✅ | emir borsada mı bilinmiyor → pozisyon ikiye katlanabilir |
| `flatten` başarısız | ✅ | kapatma yapılmamış olabilir → stop emniyeti tek katman |
| WS reconnect | ❌ | veri akışı kesintisi; karar etkilenmez (state REST'ten gelir) |
| retry'da **başarıya ulaşan** tekil REST timeout | ❌ | `signing::post` zaten geri çekip yeniden yolluyor |
| stop-yerleştirme hatası | ❌ | `ensure_sl` + `sync_exchange_sl` + `dedupe_sl` halleder |
| 429-after-backoff başarı | ❌ | backoff sonrası başarı = normal |

Emin olunamayan hata → **Critical** (güvenli yön varsayılanı).

**`ExitsOnly`** (S8'in kalan maddesi): akış kanalı kapandığında `5 dk'da >10`
kopma → kill **değil**, `ExitsOnly`. `engine.entries_allowed = false` →
`Action::Place` (yeni giriş) elenir; **`ExitNow` (çıkış) ve borsa stop'ları
etkilenmez** — mod yalnız "daha fazla risk almayı" kapatır.

**Flatten başarısız olsa bile bot durur** ve `CRITICAL` loglar:
> `düzleştirme sonrası N pozisyon AÇIK. Bot duruyor — bu pozisyonlar ARTIK
> BORSA STOP'UNA EMANET (bot içi kontrol yok). Elle müdahale gerek.`

ENV: `ENGINE_CRIT_THRESHOLD` (varsayılan 5).

### 8.4.7 Cloid ölçümü (P2-H)

Testnet'te **ölçüldü**: `place → cancel → AYNI cloid ile tekrar place`
→ **KABUL** (yeni oid). Yani cloid borsada **tek kullanımlık değil**,
`tid` gibi bir dedup anahtarı değil.

Sonuç: proses-içi monotonik sayaç **EKLENMEDİ** — eklemek idempotency
amacını bozardı (retry farklı cloid gönderirse aynı emri iki kez yollayabilir
hale gelirdi). Doğrulandı: `signing::post` **aynı gövdeyi** tekrar yollar
(`exchange_action` nonce'ı imzalamadan ÖNCE üretir, `post` aynı `body`'yi
retry eder) → retry asla farklı cloid üretmez.

Batch içi çakışma savunması: `salt = n_cycles<<16 | batch_index`.

### 8.4.8 Dürüst kısıt

**Testnet dolum kalitesi gerçek likiditeyi temsil etmez** — istatistikler
**yön göstergesidir**, kâr kanıtı değildir. Canlı eğitim tamamlanana kadar
**kâr iddiası yoktur**.

Mainnet'e geçişte `track_state.json` **taşınır mı** = **kullanıcı kararı**
(uygulama bu maddede yok).

---

## 9. CANLI DOĞRULAMA TARİHİ (mainnet — gerçek veri)

| Evre | Test | Sonuç |
|---|---|---|
| api | SDK 0.24 imza cross-check | r/s/v bayt-bayt aynı |
| api | tüm info metotları + WS 4 kanal | ✓ |
| api | CANLI EMİR (far-below limit, aç+iptal) | oid=561610126917, 561707937166 — maliyetsiz |
| api/engine | hesap sorgusu | $117.13, 135 fill, master sync |
| data | evren 178 aktif coin, tick kaybı | 6.246 tick/20 sn, 0 kayıp |
| data | CH/Redis | 12.142 tick + 4.321 mum/coin arşiv + 1.131 mid cache |
| math | sıfır-sapma | 5.828 değer, maks 5.9e-14 |
| engine | state machine 6 senaryo + alloc 4 + track 20 + ensemble 4 + cloid 2 + stop-px 2 + sl-side 4 + trailing 8 + EV 6 | 69/69 ✓ |
| engine | KIYAMET (bozuk coin + stale + resync + kötü boyut + korelasyon çöküşü) | 0 panik; graceful skip; risk kapısı aktif |

Detaylı kıyamet senaryo denetimi: `docs/KIYAMET-AUDIT.md`.

---

*Son güncelleme: 2026-10-02. Derleme: `cargo build --release --workspace` temiz, `cargo test --release --workspace`: engine 69, strategy 6, hepsi yeşil.*
