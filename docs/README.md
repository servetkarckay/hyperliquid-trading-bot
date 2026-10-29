# Dokümantasyon — hyperliquid-trading-bot

Hızlı referans. Tam mimari için `SISTEM-MIMARISI.md`.

## Modüller (registry = 1)

- **MRE** — Mean-Reversion. Katmanlar: Regime × De Bondt-Thaler Z(±2.5) × GARCH(1,1) σ≥1.0 × CS rank extreme × yönlü infaz mumu (LONG: aralık≤2.7×ATR / SHORT: aralık>2.7×ATR). SL 1.5×ATR / TP c[last−6].

MMT ve SMC-L koddan kaldırıldı (git geçmişinde durur): MMT canlıda hiç işlem
açlamadı (LEARN n=0); SMC-L'nin WS "trades" beslemesi veri göndermiyordu.

## Tuning özeti

- MRE: kanonik korundu (z=2.5, σ=1.0, SL=1.5); infaz mumu kuralı eklendi (pf 1.41→1.95 backtest).
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
