# Dokümantasyon — hyperliquid-trading-bot

Hızlı referans. Tam mimari için `SISTEM-MIMARISI.md`.

## Modüller (registry = 3)

- **MMT** — Trend & Breakout. Katmanlar: TrendBoost(4h) × Turtle20 × Dow HH/HL × Squeeze × Wyckoff SOS × Elliott × Classic confluence. SL 1.5N / TP 5N (güçlü trendde 6N=4R).
- **SMC-L** — Liquidity & Reversal. Katmanlar: Regime × Sweep∧ΔOI<0(≥%1.5, 1h pencere) × CISD∧OFI × L2 imbalance. Kanonik eşikler (0.5/0.7/1.5).
- **MRE** — Mean-Reversion. Katmanlar: Regime × De Bondt-Thaler Z(±2.5) × GARCH(1,1) σ≥1.0 × CS rank extreme. SL 1.5×ATR / TP c[last−6].

## Tuning özeti

- MMT: SL 2→1.5, TP 3→5 (kanonik replay +2.5R); güçlü trendde 4R.
- SMC-L: kanonik korundu; ΔOI %1.5 (önceki %0.2 — per-tick gürültü); shorts param bug düzeltildi.
- MRE: kanonik korundu (z=2.5, σ=1.0, SL=1.5).
- Spread-gate: mevcut `max(75, 3×round_trip)` spec formülünden daha sıkı.

## Bot durumu

- **ÇALIŞIYOR** (PID 197144, watchdog aktif 5 dk, equity $66.43 temiz).
- Yeniden başlatma: `bash scripts/start_bot_mainnet.sh` (`.env` mainnet onaylı).
- Watchdog disable: `touch /tmp/opencode/watchdog.disabled`.

## Test

`cargo test --workspace` (311 test), `cargo clippy --workspace` (0), `cargo fmt --check`.

## Güvenlik

- `.env` push edilmez; chmod 600.
- Private key rotation önerilir (operatör aksiyonu).
- Runtime state dosyaları repoya girmez.
- `ready_for_live` otomatik canlı işlem demek değildir.
