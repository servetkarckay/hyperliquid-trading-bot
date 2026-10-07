#!/usr/bin/env bash
# Bot başlatma — temiz başlatıcı.
#   kullanım: scripts/start_bot.sh
# Tüm yapılandırma `.env`'de (bot dotenvy ile kendisi yükler);
# burada env tanımı YOK — tek kaynak `.env`.
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

# Bot süresiz çalışır; durdurma kararı operatördedir.
# setsid: kendi session'ı — başlatan shell/arka plan işi temizlenirse
# process-group kill'i botu öldüremez (sessiz ölüm yasağı).
setsid nohup "$ROOT/target/release/bot" >"$LOG" 2>&1 </dev/null &
sleep 3
pgrep -af "$ROOT/target/release/bot"
