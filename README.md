# hyperliquid-trading-bot

Hyperliquid perpetual borsası üzerinde çalışan, öğrenme-temelli algoritmik işlem botu.
Registry **tek temel modüle** indirgenmiştir: **MRE** (Mean-Reversion, yönlü infaz
mumu filtresiyle). MMT (Trend & Breakout) ve SMC-L (Liquidity & Reversal) koddan
kaldırıldı — MMT canlıda hiç işlem açlamadı (LEARN n=0); SMC-L'nin Hyperliquid WS
"trades" channel'i veri göndermiyordu. İkisi de git geçmişinde durur
(silinmeden önceki commit'ler); gerektiğinde geri getirilebilir. Sermaye/risk
katmanı (Kelly sizing, funding carry) registry'den ayrıdır.

## Modüller

| Modül | Katmanlar | SL / TP (ATR) | Durum |
|---|---|---|---|
| MRE | Regime × De Bondt-Thaler Z(±2.5) × GARCH(1,1) σ≥1.0 × CS rank extreme × yönlü infaz mumu (LONG: aralık≤2.7×ATR / SHORT: aralık>2.7×ATR) | 1.5×ATR / c[last−6] | Aktif |

`STRATEGY_COUNT = 1`, cluster sayısı 1 (MeanReversion).

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
