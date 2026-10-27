# hyperliquid-trading-bot

Hyperliquid perpetual borsası üzerinde çalışan, öğrenme-temelli algoritmik işlem botu.
Registry **3 temel modüle** indirgenmiştir: **MMT** (Trend & Breakout), **SMC-L**
(Liquidity & Reversal), **MRE** (Mean-Reversion). Sermaye/risk katmanı (Kelly
sizing, funding carry) registry'den ayrılmıştır.

## Modüller

| Modül | Katmanlar | SL / TP (ATR) | Durum |
|---|---|---|---|
| MMT | TrendBoost(4h) × Turtle20 × Dow HH/HL × Squeeze × Wyckoff SOS × Elliott × Classic confluence (EMA20>EMA50 ∧ RSI>50) | 1.5N / 5N | Tuned |
| SMC-L | Regime × Sweep∧ΔOI<0 × CISD∧OFI × L2 imbalance | sweep-level / 1.5R min | Kanonik (L2 canlı feed) |
| MRE | Regime × De Bondt-Thaler Z(±2.5) × GARCH(1,1) σ≥1.0 × CS rank extreme | 1.5×ATR / c[last−6] | Kanonik |

`STRATEGY_COUNT = 3`, cluster sayısı 3 (Trend / Liquidation / MeanReversion).

## Build & test

```bash
cargo build --release -p engine --bin bot
cargo test --workspace          # 307+ test, 0 fail
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

## Durum (2026-10-27)

- Bot: **DURDURULMUŞ** (PID killed, watchdog kaldırılmış, equity $66.88 temiz).
- Son commit: `2b23c15` — parametrize + data-driven tuning pass.
- MMT kanonik replay: 30 işlem, ΣR −9.82 (önceki −12.32).
- MRE kanonik replay: 2445 işlem, ΣR +121.8, +0.050R/işlem.
- `.env` ve private key repoya girmez (`.gitignore`).

## Dokümantasyon

- `docs/SISTEM-MIMARISI.md` — tam mimari (katmanlar, kontratlar, execution, öğrenme).
- `docs/README.md` — hızlı referans (bu dosyanın özeti).

## Güvenlik

- `.env` hiçbir zaman push edilmez.
- Private key chat'e yapıştırılmıştı; rotation önerilir.
- State dosyaları (`config/*.json`, `testnet_final/*`) runtime'da üretilir; repoya girmez.
