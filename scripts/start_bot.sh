#!/usr/bin/env bash
# Bot başlatma — canlı kullanım (testnet).
#   kullanım: scripts/start_bot.sh
# Önceki koşunun log'unu <log>.prev altına arşivler.
set -euo pipefail

ROOT=/root/hyperliquid
LOG=/tmp/opencode/bot.log
cd "$ROOT"

# Kendi komut satırıyla eşleşip pkill'in shell'i öldürmesini önlemek için
# script yolu üzerinden eşleşme yapılır.
if pgrep -f "$ROOT/target/release/bot" >/dev/null 2>&1; then
    echo "bot zaten çalışıyor:"
    pgrep -af "$ROOT/target/release/bot"
    exit 1
fi

[ -f "$LOG" ] && cp "$LOG" "$LOG.prev"

# Sıfırlanacak dosya yok: `config/track_state.json` öğrenilmiş ağırlıkları
# taşır, silinmez.
export RUST_LOG=info
export ENGINE_BUDGET_USD=300
export ENGINE_CYCLE_SECS=300
export ENGINE_FLATTEN=1
export ENGINE_HIST_BARS=4400
export ENGINE_INTERVAL=3600
export ENGINE_LEVERAGE=5
export ENGINE_MIN_SCORE=0.30
export ENGINE_TOP_N=3
export TRAIL_ATR_K=1.5
export TRAIL_ON=1
# ENGINE_COINS verilmiyor → tüm aktif evren (P1-D: tek kaynak)

nohup timeout 28800 "$ROOT/target/release/bot" >"$LOG" 2>&1 </dev/null &
sleep 3
pgrep -af "$ROOT/target/release/bot"