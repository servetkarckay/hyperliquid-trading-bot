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

# ── DEPOLAMA ÖN KONTROLÜ ──────────────────────────────────────────────
# Bot'un doğru çalışması mum depozisinin ayakta olması ZORUNLUDUR.
# (Denetim bulgusu: CH sunucusu durunca stratejiler veri alamaz →
# SKIP → emir ÜRETİLMEZ; ayrıca motor logunda hata sesi OLMAZ.
# Bu yüzden start öncesi sağılık doğrulanır; yoksa sunucu kaldırılır.)
CH_URL=$(grep -E "^CLICKHOUSE_URL" "$ROOT/.env" | cut -d= -f2)
CH_URL=${CH_URL:-http://localhost:8123}
ch_ping() { curl -so /dev/null -w '%{http_code}' --max-time 4 "$CH_URL/" --data-binary "SELECT 1"; }
if ! [ "$(ch_ping)" = "200" ]; then
    echo "depolar ölü — sunucu başlatılıyor ($CH_URL)"
    CH_BIN=$(ls /root/.clickhouse/versions/*/clickhouse 2>/dev/null | sort -V | tail -1)
    if [ -n "$CH_BIN" ]; then
        setsid nohup "$CH_BIN" server --config-file /tmp/opencode/ch/config.xml \
            > /tmp/opencode/ch/server.out 2>&1 </dev/null &
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            sleep 2
            [ "$(ch_ping)" = "200" ] && break
        done
    fi
    if ! [ "$(ch_ping)" = "200" ]; then
        echo "HATA: depo ($CH_URL) ayağa kalkamadı — bot başlatılmaz"
        exit 1
    fi
fi
echo "depo sağlıklı: $CH_URL"

# Sıfırlanacak dosya yok: `config/track_state.json` öğrenilmiş ağırlıkları
# taşır, silinmez.

# Bot süresiz çalışır; durdurma kararı operatördedir.
# setsid: kendi session'ı — başlatan shell/arka plan işi temizlenirse
# process-group kill'i botu öldüremez (sessiz ölüm yasağı).
setsid nohup "$ROOT/target/release/bot" >"$LOG" 2>&1 </dev/null &
sleep 3
pgrep -af "$ROOT/target/release/bot"
