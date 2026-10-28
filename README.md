# hyperliquid-trading-bot

Hyperliquid perpetual borsası üzerinde çalışan, öğrenme-temelli algoritmik işlem botu.
Registry **3 temel modüle** indirgenmiştir: **MMT** (Trend & Breakout), **SMC-L**
(Liquidity & Reversal), **MRE** (Mean-Reversion). Sermaye/risk katmanı (Kelly
sizing, funding carry) registry'den ayrılmıştır.

## Modüller

| Modül | Katmanlar | SL / TP (ATR) | Durum |
|---|---|---|---|
| MMT | TrendBoost(4h) × Turtle20 × Dow HH/HL × Squeeze × Wyckoff SOS × Elliott × Classic confluence (EMA20>EMA50 ∧ RSI>50) | 1.5N / 5N (güçlü trendde 6N=4R) | Tuned |
| SMC-L | Regime × Sweep∧ΔOI<0(≥%1.5, 1h pencere) × CISD∧OFI × L2 imbalance | sweep-level / 1.5R min | Kanonik (L2 canlı feed) |
| MRE | Regime × De Bondt-Thaler Z(±2.5) × GARCH(1,1) σ≥1.0 × CS rank extreme | 1.5×ATR / c[last−6] | Kanonik |

`STRATEGY_COUNT = 3`, cluster sayısı 3 (Trend / Liquidation / MeanReversion).

## Build & test

```bash
cargo build --release -p engine --bin bot
cargo test --workspace          # 311 test, 0 fail
cargo clippy --workspace        # 0 warning
cargo fmt --check
```

## Çalıştırma

```bash
# mainnet (operatör onayı gerekli)
bash scripts/start_bot_mainnet.sh
```

Ön koşul: `.env` → `NETWORK=mainnet`, `MAINNET_CONFIRMED=1`, geçerli key'ler.
Bot DURDURULMUŞ durumdan başlatılır; risk_state temiz → peak=current equity.

Watchdog: `*/5 * * * * /root/hyperliquid/scripts/watchdog.sh` (PIDFILE tabanlı,
disable lock: `/tmp/opencode/watchdog.disabled`).

## Durum (2026-10-28)

- Bot: **ÇALIŞIYOR** (PID 197144, watchdog aktif 5 dk, equity $66.43 temiz).
- Son commit: `813215a` — Party 1-3 (peak settled-only, R unification, SMC-L
  ΔOI %1.5 windowed, MMT TP 5N/4R strong, spread-gate pin, ops hardening).
- MMT kanonik replay: 30 işlem, ΣR −9.82 (önceki −12.32).
- MRE kanonik replay: 2445 işlem, ΣR +121.8, +0.050R/işlem.
- Live: MRE n=2, win 50%, pf 6.69, ΣR=+0.64; MMT/SMC-L n=0.
- ClickHouse: ayakta (localhost:8123, 5 tablo); bot canlı yolda kullanmaz
  (arşiv/backfill).
- `.env` ve private key repoya girmez (`.gitignore`); `.env` chmod 600.

## Dokümantasyon

- `docs/SISTEM-MIMARISI.md` — tam mimari (katmanlar, kontratlar, execution, öğrenme).
- `docs/README.md` — hızlı referans.

## Güvenlik

- `.env` hiçbir zaman push edilmez; chmod 600.
- Private key chat'e yapıştırılmıştı; **rotation önerilir** (operatör aksiyonu).
- State dosyaları (`config/*.json`, `config/*.jsonl`, `testnet_final/*`) runtime'da
  üretilir; repoya girmez.
- `Config` Debug çıktısı private_key'i maskeler.
- `ready_for_live` otomatik canlı işlem demek değildir (bkz. docs §26).
