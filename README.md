# Hyperliquid Trading Bot

Hyperliquid üzerinde çalışan, tek stratejili (MRE) canlı trading botu. Rust workspace: `api`, `data`, `engine`, `math`, `strategy`, `utils`.

## Ne yapar

Bot, Hyperliquid evrenindeki tüm perp'leri (≈179 coin) 1h mumlar üzerinde tarar ve **Mean-Reversion & Overreaction Engine (MRE)** stratejisiyle aşırı sapma anlarında kontra pozisyon alır. Her 5 dakikada bir tam evren taraması + tahsis + yürütme döngüsü çalışır. Pozisyonlar borsada asılı SL/TP trigger'larıyla korunur; trailing stop ratchet uygulanır. Öğrenme, net performans (fee + gerçek USDC funding dahil) üzerinden R-bazlı ağırlık güncellemesi yapar.

## ⚠️ Sorumluluk reddi

Bu bot **eğitim ve araştırma amaçlıdır**. Finansal tavsiye değildir. Kullanım riski size aittir. Gerçek para ile çalıştırmadan önce testnet'te doğrulayın. Yazarlar, botun kullanımından doğan hiçbir kayıptan sorumlu değildir.

## Kurulum

```bash
# 1. Klonla
git clone https://github.com/servetkarckay/hyperliquid-trading-bot.git
cd hyperliquid-trading-bot

# 2. Konfigürasyon
cp .env.example .env
# .env'i düzenle: NETWORK, HYPERLIQUID_API_URL, HYPERLIQUID_WS_URL,
# HYPERLIQUID_PRIVATE_KEY, API_WALLET_ADDRESS, USER_ADDRESS, ...

# 3. Derle
cargo build --release --workspace

# 4. Testnet'te doğrula (E2E)
gh workflow run testnet-e2e.yml -f confirm_testnet=TESTNET

# 5. Çalıştır
bash scripts/start_bot_mainnet.sh
```

## Mimari

```
Hyperliquid (WS + REST)
        │
        ▼
┌────────────────┐     ┌──────────────┐
│  data crate    │────▶│ engine_loop  │  (tarama → tahsis → yürütme → çıkış → öğrenme)
│  (mum agg,     │     └──────┬───────┘
│   bağlam,      │            │
│   backfill)    │            ▼
└────────────────┘     ┌──────────────┐
                       │ allocation   │  (sermaye tahsisi, TEK maliyet modeli)
                       │ manager      │
                       └──────┬───────┘
                              │
                              ▼
                       ┌──────────────┐
                       │  executor    │──▶ Hyperliquid (emir/iptal/SL-TP)
                       │  (borsa I/O) │
                       └──────────────┘
```

- `api/` — REST/WS istemcisi, EIP-712 imza, endpoint ayrıştırma
- `data/` — WS ingestor, mum agregasyonu, backfill (REST candleSnapshot/funding geçmişi), bağlam
- `engine/` — döngü, tahsis, executor, risk manager, state machine, reconciliation, öğrenme
- `math/` — göstergeler (ATR, RSI, MACD, Bollinger, GARCH, ADX, OBV, VWAP)
- `strategy/` — MRE motoru + replay koşucusu
- `utils/` — zaman, logger

## Strateji: MRE (6 katmanlı kontra zinciri)

Bir katman veto → sinyal yok. Tüm katmanlar deterministik ve replay-idempotent.

1. **Rejim**: `classify` TREND ve Blocked (VolSpike/Squeeze) rejimleri red; MeanReversion/GEÇİŞ onaylı. Trend piyasasında kontra uygulanmaz.
2. **De Bondt-Thaler Z**: 6-bar aşırı hareket, 100-bar log-return std normalize. Z ≤ −2.5 panik (kontra LONG) / Z ≥ +2.5 euphoria (kontra SHORT).
3. **GARCH σ teyidi**: GARCH(1,1) koşullu volatilite — σ_next/σ_long-run ≥ 1.0: anomali volatil patlama yaratsın.
4. **Cross-sectional rank**: kesit quintile. Panik uzvu evrenin alt %40'ında (Bottom/BottomShort), euphoria üst %20'de (Top). Orta bölgeler → Hold.
5. **İnfaz mumu kuralı (yönlü)**: sinyal barı aralığı/ATR14. LONG'da infaz mumu dinmiş olmalı (aralık ≤ 2.7×ATR); SHORT'ta infaz mumu zirvede olmalı (aralık > 2.7×ATR).
6. **PANİK-DİP LONG**: LONG yalnız gerçek panik dipinde (son 6 bar kümülatif getiri ≤ −6.3%). Sıradan dip alma pahalıya düşüş piyasasında birikir. SHORT'a uygulanmaz.

Fiyat planı: hedef = aşırı hareketin başlangıcı (c[last−6]) — ortalamaya dönüş hedefi; SL = 1.5×ATR14 risk tavanı; TP = c[last−6].

## TradeIntent (kanonik niyet)

MRE'nin ürettiği `TradeIntent { long, entry, sl, tp, strength, note, signal_ts, signal_candle_ts, expires_at }` hem canlı engine hem replay tarafından **aynı** kullanılır. `expires_at = signal_candle_ts + 6h`. Girişte geçerlilik süresi kontrol edilir. `apply_real_fill` intent TP'sini ezmez (`tp_from_intent` bayrağı).

## Risk yönetimi

- `RiskManager`: peak_equity / current_equity / halted. %20 drawdown → halt.
- Risk state JSON (`config/risk_state.json`) — bozuk JSON veya anlamsız değer (peak≤0, current<0, boş halted) → Corrupt → mainnet başlangıcı reddedilir.
- Mainnet ilk açılışta risk state yoksa: `RISK_STATE_INIT_CONFIRMED=1` + açık pozisyon kontrolü gerekir (operatör onayı).
- Save hatası → `mark_save_failed()` + gate lock → yeni girişler kilitlenir.
- Coin başına pozisyon tavanı + toplam maruziyet limiti (position_value üzerinden).

## Güvenlik modeli

- **Corrupt risk state → mainnet başlangıcı reddedilir** (bail).
- **REST başarısızlığı asla "veri yok" kanıtı olamaz**: `recon_decide` → `ReconAction::Unknown` — pending + lock korunur, Flat YASAK.
- **Stale stop OID**: `sync_exchange_sl` her döngüde borsa gerçeğine göre temizler; sorgu başarısızsa girişler kilitlenir.
- **SHORT recovery**: yön `pos.szi` işaretinden çıkarılır; SL/TP yön sözleşmesi doğrulanır.
- **openOrders şeması**: `frontendOpenOrders` kullanılır (reduceOnly, origSz, cloid); her satır zorunlu alanlarla doğrulanır; tek bozuk satır → tüm sorgu Err.
- **Pozisyon sorgusu**: atomik temp map; tüm satırlar doğrulanır (coin, szi finite, entryPx finite>0, positionValue finite>0); bozuk satır → Err, eski map korunur.
- **CLOID recovery**: `orderStatus(cloid)` → `order.order.oid` yolu okunur; `open` → OID bağlanır; `unknownOid` → verified None; diğer → None; şema bozuk → Err.
- **JSON şema doğrulaması**: openOrders yanıtı dizi değilse Err.

## Fee ve funding muhasebesi

- `userFills` fee'si her fill'de düşülür; açılış fee'si de pozisyonun net R'ına eklenir.
- `userFunding` gerçek USDC hareketleri (fundingHistory piyasa oranları değil) — `delta.coin`/`delta.usdc` yolu.
- R = (closedPnl − fee + funding) / risk_usd (her iki çıkış yolunda).
- Funding kayıtları pozisyon ömrü (open_ts..close_ts) ile ilişkilendirilir; restart/contamination guard var.

## Canlı / replay hizası

- Canlı tarama **sadece kapanmış mumlar** kullanır.
- `TradeIntent.expires_at` girişte kontrol edilir.
- Replay (`strategy/bin/mre_replay.rs`) aynı `plan_replay` zincirini kullanır; ufuk (horizon bar) parametresiyle çalışır.

## Replay sonuçları (79 coin, 190 gün, 1h)

| Konfigürasyon | İşlem | ΣR | pf | MDD |
|---|---|---|---|---|
| Filtresiz MRE | 465 | +90.0 | 1.28 | 62.3R |
| Panic-dip LONG + tüm SHORT | 239 | +88.7 | 1.56 | 19.0R |

- Ufuk kararlı: 48/96/144/192 bar → ΣR 88-90, pf ~1.28.
- Maliyet dayanıklı: fee ×3'te bile pf 1.12.
- Günlük işlem sıklığı: ortalama ~1.3 işlem/gün, %42 gün sessiz, aktif günlerde medyan 1, rekor 14.

## İşletme

- Başlat: `bash scripts/start_bot_mainnet.sh` (pidfile: `logs/bot-mainnet.pid`)
- Log: `sed 's/\x1b\[[0-9;]*m//g' logs/bot-mainnet.log`
- Watchdog: `scripts/watchdog.sh` (yeniden başlatmaları yönetir; devre dışı bırakma mekanizması script içinde belgelenmiştir)
- State: `config/risk_state.json`, `config/track_state.json`, `config/trade_exits.jsonl`

## CI

- **PR-CI**: fmt, check, clippy (-D warnings), test, debug+release build — her push/PR.
- **testnet-e2e**: manuel dispatch, gerçek testnet emir → doğrula → iptal → flat doğrula. Guard: mainnet izi YOK, secret format denetimi.
- **probe_smoke**: E2E'nin gerçek emir adımı.

## Testler

- 296+ test, 0 hata. clippy 0 uyarı. fmt temiz.

## Bilinen sınırlar

- Replay entry timing: sinyal barı → sonraki bar açılışında giriş (next-open) vs canlı IOC.
- PF iddiaları basitleştirilmiş maliyet replay'inden; canlı doğrulama birikimli.
- Private key rotation operatör aksiyonu.