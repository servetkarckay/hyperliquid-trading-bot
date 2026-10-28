#!/usr/bin/env bash
# WATCHDOG: bot process'i yoksa start_bot_mainnet.sh ile geri başlatır.
# Sessiz ölüm (OOM killer, WS kopması vs.) → otomatik kurtarma.
# crontab: */5 * * * * /root/hyperliquid/scripts/watchdog.sh
#
# Güvenlik:
# - Birden fazla bot (testnet+mainnet) aynı binary'yi paylaşır; pgrep -f
#   ikisini de eşleştirirdi → mainnet ölüyken testnet çalışıyorsa watchdog
#   "ok" sanıp mainnet'i yeniden başlatmıyordu. PIDFILE tabanlı kontrol
#   kullanılır: mainnet botu start_bot_mainnet.sh tarafından yazılır.
# - Operatörün bilinçli olarak botu durdurma isteği için lock dosyası:
#   `/tmp/opencode/watchdog.disabled` varsa watchdog restart YAPMAZ.
ROOT=/root/hyperliquid
LOG=/tmp/opencode/watchdog-mainnet.log
PIDFILE=/tmp/opencode/bot-mainnet.pid
DISABLED=/tmp/opencode/watchdog.disabled
ts() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }
if [ -f "$DISABLED" ]; then
    echo "$(ts) disabled (lock: $DISABLED)" >> "$LOG"
    exit 0
fi
alive=false
if [ -f "$PIDFILE" ]; then
    pid="$(cat "$PIDFILE" 2>/dev/null)"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        alive=true
    fi
fi
if [ "$alive" = true ]; then
    echo "$(ts) ok (pid=$pid)" >> "$LOG"
    exit 0
fi
echo "$(ts) mainnet bot yok — restart" >> "$LOG"
"$ROOT/scripts/start_bot_mainnet.sh" >> "$LOG" 2>&1 || echo "$(ts) restart BAŞARISIZ" >> "$LOG"