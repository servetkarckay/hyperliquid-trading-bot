# Hyperliquid Trading Bot — MRE (Mean-Reversion & Overreaction Engine)

Canlı (mainnet) Hyperliquid trading botu. Tek strateji: **MRE** — De Bondt-Thaler aşırı tepki teorisine dayalı, 6 katmanlı bir kontra (mean-reversion) motoru. Rust workspace (api, data, engine, math, strategy, utils).

## Özet

- **Strateji**: MRE — piyasa panik/euforia anlarında aşırı sapmayı yakalar, 6-bar aşırı hareketin başlangıcına (c[last−6]) geri dönüşü hedefler.
- **Evren**: `ENGINE_COINS=all` — Hyperliquid evrenindeki tüm perp'ler (~179 coin).
- **Zaman dilimi**: 1h mumlar, 5 dakikalık döngü.
- **Risk**: coin başına %1 risk, %20 drawdown halt, %100 likidasyon mesafesi koruması.
- **Öğrenme**: net performans (closedPnl − fee − gerçek USDC funding) ile R-bazlı.

## Mimari

```
┌─────────┐   WS   ┌──────────┐  5dk döngü  ┌────────────┐
│Hyperliquid│ ────▶ │ data crate │ ─────────▶ │ engine_loop │
│  (REST)  │        │ (mum agg) │            │ (tarama)   │
└─────────┘        └──────────┘            └─────┬──────┘
                                                  │
                      ┌───────────────┐           │
                      │ AllocationMgr │◀──────────┤
                      │ (pozisyon     │           │
                      │  boyutu)      │           │
                      └───────┬───────┘           │
                              │                   ▼
                              │            ┌────────────┐
                              └───────────▶│ executor   │──▶ Hyperliquid
                                           │ (emir)     │
                                           └────────────┘
```

- `api/` — REST/WS istemcisi, signing, endpoint ayrıştırma
- `data/` — WS ingestor, mum agregasyonu, backfill, storage
- `engine/` — döngü, allocation, executor, risk manager, state machine, reconciliation
- `math/` — göstergeler (ATR, RSI, MACD, Bollinger, GARCH, ADX, OBV, VWAP)
- `strategy/` — MRE motoru + replay koşucusu
- `utils/` — zaman, logger

## MRE strateji katmanları (6 katmanlı kontra zinciri)

Bir katman veto → sinyal yok:

1. **Rejim**: `classify` TREND ve Blocked (VolSpike/Squeeze) rejimleri RED; MeanReversion/GEÇİŞ onaylı.
2. **De Bondt-Thaler Z**: 6-bar aşırı hareket, 100-bar log-return std normalize. Z ≤ −2.5 panik (kontra LONG) / Z ≥ +2.5 euphoria (kontra SHORT).
3. **GARCH σ teyidi**: GARCH(1,1) koşullu volatilite — σ_next/σ_long-run ≥ 1.0: anomali volatil patlama yaratsın.
4. **Cross-sectional rank**: kesit quintile — panik uzvu → evrenin alt %40'ı (Bottom/BottomShort), euphoria → üst %20 (Top). Orta bölgeler → Hold.
5. **İnfaz mumu kuralı (yönlü)**: sinyal barı aralığı/ATR14. LONG'da infaz mumu DİNMIŞ olmalı (aralık ≤ 2.7×ATR — bıçak yere düşmüş); SHORT'ta infaz mumu ZİRVEDE olmalı (aralık > 2.7×ATR — coşku tam delirmede).
6. **PANİK-DİP LONG**: LONG yalnız gerçek panik dipinde (son 6 bar kümülatif getiri ≤ −6.3%). Sıradan dip alma pahalıya düşüş piyasasında birikir. Out-of-sample teyit edildi (aşağıda).

Fiyat planı: hedef = aşırı hareketin başlangıcı (c[last−6]) — "ortalamaya dönüş" hedefi; SL = 1.5×ATR14 risk tavanı. TP = c[last−6].

## TradeIntent (kanonik niyet)

MRE'nin ürettiği `TradeIntent { long, entry, sl, tp, strength, note, signal_ts, signal_candle_ts, expires_at }` hem canlı engine hem replay tarafından **aynı** kullanılır. `expires_at = signal_candle_ts + 6h`. Girişte geçerlilik süresi kontrol edilir. `apply_real_fill` intent TP'sini ezmez (`tp_from_intent` bayrağı).

## Risk yönetimi

- `RiskManager`: peak_equity / current_equity / halted. %20 drawdown → halt.
- Risk state JSON (`config/risk_state.json`) — bozuk JSON / anlamsız değer (peak≤0 vb.) → Corrupt → mainnet başlangıcı reddedilir.
- Mainnet ilk açılışta risk state yoksa: `RISK_STATE_INIT_CONFIRMED=1` + açık pozisyon kontrolü gerekir (operatör onayı).
- Save hatası → `mark_save_failed()` + gate lock → yeni girişler kilitlenir.
- Coin başına pozisyon tavanı + toplam maruziyet limiti (position_value üzerinden).

## Güvenlik modeli

- **Corrupt risk state → mainnet başlangıcı reddedilir** (bail).
- **REST başarısızlığı asla "veri yok" kanıtı olamaz**: `recon_decide` → `ReconAction::Unknown` — pending + lock korunur, Flat YASAK.
- **Stale stop OID**: `sync_exchange_sl` her döngüde borsa gerçeğine göre temizler; sorgu başarısızsa girişler kilitlenir.
- **SHORT recovery**: yön `pos.szi` işaretinden çıkarılır (hardcoded LONG yok); SL/TP yön sözleşmesi doğrulanır.
- **openOrders şeması**: `frontendOpenOrders` kullanılır (reduceOnly, origSz, cloid); her satır zorunlu alanlarla doğrulanır; tek bozuk satır → tüm sorgu Err.
- **Pozisyon sorgusu**: atomik temp map; tüm satırlar doğrulanır (coin, szi finite, entryPx finite>0, positionValue finite); bozuk satır → Err, eski map korunur.
- **CLOID recovery**: `orderStatus(cloid)` → `order.order.oid` yolu okunur; `open` → OID bağlanır; `unknownOid` → verified None; diğer → None; şema bozuk → Err.
- **JSON şema doğrulaması**: openOrders yanıtı dizi değilse Err (unwrap_or_default YOK).

## Fee ve funding muhasebesi

- `userFills` fee'si her fill'de düşülür; açılış fee'si de pozisyonun net R'ına eklenir.
- `userFunding` gerçek USDC hareketleri (fundingHistory piyasa oranları DEĞİL) — `delta.coin`/`delta.usdc` yolu.
- R = (closedPnl − fee + funding) / risk_usd (her iki çıkış yolunda).
- Funding kayıtları pozisyon ömrü (open_ts..close_ts) ile ilişkilendirilir; restart/contamination guard var.

## Canlı / replay hizası

- Canlı tarama **sadece kapanmış mumlar** kullanır (açık mum push'u yok).
- `TradeIntent.expires_at` girişte kontrol edilir.
- Replay (`strategy/bin/mre_replay.rs`) aynı `plan_replay` zincirini kullanır; ufuk (horizon bar) parametresiyle çalışır.

## Replay bulguları (taze veri: 79 coin, 190 gün, 1h)

| Konfigürasyon | İşlem | ΣR | pf | MDD |
|---|---|---|---|---|
| Filtresiz MRE | 465 | +90.0 | 1.28 | 62.3R (%47) ⚠️ |
| **Panic-dip LONG + tüm SHORT** | 239 | +88.7 | 1.56 | 19.0R (~%17) ✓ |

- Ufuk kararlı: 48/96/144/192 bar → ΣR 88-90, pf ~1.28.
- Maliyet dayanıklı: fee ×3'te bile pf 1.12.
- Zamansal kararlılık filtreyle sağlanır: filtresizde May-Tem kayıp (LONG pf 0.48-0.65); filtreyle May-Tem −2.8R (−51.3R'den), Ağu-Eki pf 1.99.
- OBV akış bulgusu bu veride tekrarlanmadı — kullanılmıyor.
- SHORT pf 1.48 bu veride (eski 8-coin sette "kurtarılamaz" denmişti — veri setine bağlı).

Günlük işlem sıklığı: ortalama ~1.3 işlem/gün, %42 gün sessiz, aktif günlerde medyan 1, rekor 14 (volatil günlerde). Canlı tahmin: 1-5/gün, volatil günlerde 10+.

## İşletme

- Başlat: `bash scripts/start_bot_mainnet.sh` (pidfile: `logs/bot-mainnet.pid`)
- Log: `sed 's/\x1b\[[0-9;]*m//g' logs/bot-mainnet.log`
- Watchdog: `scripts/watchdog.sh` (devre dışı bırakma: `touch /tmp/opencode/watchdog.disabled`)
- Restart dance: watchdog.disable → kill → start → enable
- State: `config/risk_state.json`, `config/track_state.json`, `config/trade_exits.jsonl`

## CI

- **PR-CI** (`.github/workflows/pr-ci.yml`): fmt, check, clippy (-D warnings), test, debug+release build — her push/PR.
- **testnet-e2e** (`.github/workflows/testnet-e2e.yml`): manuel dispatch, gerçek testnet emir → doğrula → iptal → flat doğrula. Guard: mainnet izi YOK, secret format denetimi. Secret'lar: `API_URL`, `WS_URL`, `TESTNET_API_WALLET_SECRET`, `TESTNET_API_WALLET_ADDRESS` (+ opsiyonel `TESTNET_USER_ADDRESS`).
- **probe_smoke** binary: E2E'nin gerçek emir adımı. `TESTNET=1 probe_smoke <COIN> [notional=20]`.

## Testler

- 296+ test, 0 hata: engine 229, data 32, api 13, strategy 20.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: 0 uyarı.
- `cargo fmt --all -- --check`: temiz.

## Bilinen sınırlar

- Replay entry timing: sinyal barı → sonraki bar açılışında giriş (next-open) vs canlı IOC (slippage farkı).
- PF iddiaları basitleştirilmiş maliyet replay'inden; canlı doğrulama birikimli.
- Regime filter "Gecis" etiketini geçirir; May-Tem kaybı panic-dip filtresiyle büyük ölçüde (−51.3R → −2.8R) çözüldü ama tamamen sıfırlanmadı.
- Private key rotation operatör aksiyonu (açık).