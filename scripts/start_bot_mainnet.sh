#!/usr/bin/env bash
# MAINNET başlatıcı — YALNIZ operatör elle çalıştırır.
# Ön koşullar:
#   1) .env mainnet-değerli (NETWORK=mainnet + MAINNET_CONFIRMED=1 + key'ler)
#   2) mainnet hesapta USDC var (bot margin'siz çalışamaz)
#   3) testnet bot ile AYRI process (state ayrımı mainnet/ dizininde)
set -euo pipefail

ROOT=/root/hyperliquid
LOG=/tmp/opencode/bot-mainnet.log
cd "$ROOT"

# PID dosyası: aynı binary iki bot (testnet+mainnet) — pidfile ile ayır
PIDFILE=/tmp/opencode/bot-mainnet.pid
if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    echo "mainnet bot zaten çalışıyor: $(cat "$PIDFILE")"
    exit 1
fi

grep -q "^NETWORK=mainnet" .env || { echo ".env NETWORK=mainnet değil"; exit 1; }
grep -q "^MAINNET_CONFIRMED=1" .env || { echo "MAINNET_CONFIRMED=1 yok"; exit 1; }

# Depo (CH arşiv) sağlık
CH_URL=$(grep -E "^CLICKHOUSE_URL" .env | cut -d= -f2)
CH_URL=${CH_URL:-http://localhost:8123}
if ! curl -so /dev/null -w '%{http_code}' --max-time 4 "$CH_URL/" --data-binary "SELECT 1" | grep -q 200; then
    echo "CH depo sağlıksız — Memory fallback ile devam (arşiv kapalı)"
fi

[ -f "$LOG" ] && cp "$LOG" "$LOG.prev"

# Ortam: .env.mainnet TAM yükle (dotenvy zaten-set edileni ezmez —
# bu yüzden TÜM operasyonel anahtarlar .env.mainnet'te tanımlı olmalı)
set -a
source .env
set +a

# setsid: kendi session'ı — başlatan shell ölse de bot yaşar
setsid nohup "$ROOT/target/release/bot" > "$LOG" 2>&1 </dev/null &
echo $! > "$PIDFILE"
sleep 3
pgrep -af "$ROOT/target/release/bot" | head -1
echo "mainnet bot başlatıldı — log: $LOG"
