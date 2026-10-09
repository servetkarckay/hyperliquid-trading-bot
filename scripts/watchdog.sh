#!/usr/bin/env bash
# WATCHDOG: bot process'i yoksa start_bot.sh ile geri başlatır.
# Sessiz ölüm ( bellek baskısı killeri vs. ) → otomatik kurtarma.
# crontab: */5 * * * * /root/hyperliquid/scripts/watchdog.sh
ROOT=/root/hyperliquid
LOG=/tmp/opencode/watchdog.log
ts() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }
if ! pgrep -f "$ROOT/target/release/bot" >/dev/null 2>&1; then
    echo "$(ts) bot yok — restart" >> "$LOG"
    "$ROOT/scripts/start_bot.sh" >> "$LOG" 2>&1 || echo "$(ts) restart BAŞARISIZ" >> "$LOG"
else
    echo "$(ts) ok" >> "$LOG"
fi
