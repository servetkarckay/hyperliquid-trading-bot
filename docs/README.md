# Dokümantasyon — hyperliquid-trading-bot

Hızlı referans. Tam mimari için `SISTEM-MIMARISI.md`.

## Modüller (registry = 3)

- **MMT** — Trend & Breakout. Katmanlar: TrendBoost(4h) × Turtle20 × Dow HH/HL × Squeeze × Wyckoff SOS × Elliott × Classic confluence. SL 1.5N / TP 5N.
- **SMC-L** — Liquidity & Reversal. Katmanlar: Regime × Sweep∧ΔOI<0 × CISD∧OFI × L2 imbalance. Kanonik eşikler (0.5/0.7/1.5).
- **MRE** — Mean-Reversion. Katmanlar: Regime × De Bondt-Thaler Z(±2.5) × GARCH(1,1) σ≥1.0 × CS rank extreme. SL 1.5×ATR / TP c[last−6].

## Tuning özeti

- MMT: SL 2→1.5, TP 3→5 (kanonik replay +2.5R).
- SMC-L: kanonik korundu (tüm sweep hücreleri negatif).
- MRE: kanonik korundu (z=2.5, σ=1.0, SL=1.5).

## Bot durumu

- **DURDURULMUŞ** (PID killed, watchdog kaldırılmış, equity $66.88 temiz).
- Yeniden başlatma: `bash scripts/start_bot_mainnet.sh` (`.env` mainnet onaylı).
- State temizlendi → cold-start.

## Test

`cargo test --workspace` (307+ test), `cargo clippy --workspace` (0), `cargo fmt --check`.

## Güvenlik

- `.env` push edilmez.
- Private key rotation önerilir.
- Runtime state dosyaları repoya girmez.
