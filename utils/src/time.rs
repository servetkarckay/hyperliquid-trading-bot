//! TIME — NTP (SNTP v4) senkronizasyonu ve RTT ölçümü.
//!
//! `DEFAULT_SERVERS` havuzundan `sync` → ofset hesaplar; `now_ms`
//! ofset-düzeltilmiş unix ms döner; `rest_rtt` borsa gecikme probu.
//! Ağ engelli ortamda None döner (saat varsayımına DÜŞMEZ — offset'siz
//! yol açıkça loglanmalı).
use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

/// NTP ile sistem saati arasındaki kayma (unix ms). 0 = senkron yapılmadı.
static NTP_OFFSET_MS: AtomicI64 = AtomicI64::new(0);

const NTP_UNIX_EPOCH_DELTA: i64 = 2_208_988_800; // 1900 -> 1970 saniye

pub const DEFAULT_SERVERS: &[&str] = &[
    "pool.ntp.org",
    "time.google.com",
    "time.cloudflare.com",
    "time.apple.com",
];

/// Düzeltilmiş unix ms: sistem saati + NTP ofseti (senkron yoksa sistem saati).
pub fn now_ms() -> u64 {
    let sys = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    (sys + NTP_OFFSET_MS.load(Ordering::Relaxed)) as u64
}

pub fn offset_ms() -> i64 {
    NTP_OFFSET_MS.load(Ordering::Relaxed)
}

fn set_offset(ms: i64) {
    NTP_OFFSET_MS.store(ms, Ordering::Relaxed);
}

/// Sunuculara paralel SNTP sorgusu; ofsetlerin MEDIAN'ını al ve global ofset olarak uygula.
pub fn sync(servers: &[&str], timeout: Duration) -> Option<i64> {
    let handles: Vec<_> = servers
        .iter()
        .map(|s| {
            let s = s.to_string();
            std::thread::spawn(move || ntp_offset_ms(&s, timeout))
        })
        .collect();
    let mut offsets: Vec<i64> = handles
        .into_iter()
        .filter_map(|h| h.join().ok()?.ok())
        .collect();
    if offsets.is_empty() {
        return None;
    }
    offsets.sort_unstable();
    let median = offsets[offsets.len() / 2];
    set_offset(median);
    Some(median)
}

/// Tek sunucuya SNTP sorgusu -> ofset (unix ms).
/// ofset = ((t2 - t1) + (t3 - t4)) / 2 (t1: gönderim, t2: sunucu alımı,
/// t3: sunucu gönderimi, t4: yerel alım)
pub fn ntp_offset_ms(server: &str, timeout: Duration) -> anyhow::Result<i64> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    let addr = format!("{server}:123")
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| anyhow::anyhow!("NTP adresi çözülemedi: {server}"))?;
    sock.connect(addr)?;

    let t1 = unix_ms();
    let mut pkt = [0u8; 48];
    pkt[0] = 0x23; // LI=0, VN=4, Mode=3 (client)
    write_ntp_ts(&mut pkt[40..48], t1);
    sock.send(&pkt)?;

    let mut buf = [0u8; 48];
    sock.recv(&mut buf)?;
    let t4 = unix_ms();
    if buf[0] & 0b111 != 4 || buf[1] == 0 {
        anyhow::bail!("geçersiz NTP yanıtı (mode/stratum)");
    }
    let t2 = read_ntp_ts(&buf[32..40]);
    let t3 = read_ntp_ts(&buf[40..48]);
    Ok(((t2 - t1) + (t3 - t4)) / 2)
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn write_ntp_ts(out: &mut [u8], unix_ms: i64) {
    let secs = unix_ms.div_euclid(1000) + NTP_UNIX_EPOCH_DELTA;
    let frac = ((unix_ms.rem_euclid(1000)) << 32) / 1000;
    out[..4].copy_from_slice(&(secs as u32).to_be_bytes());
    out[4..].copy_from_slice(&(frac as u32).to_be_bytes());
}

fn read_ntp_ts(b: &[u8]) -> i64 {
    let secs = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i64;
    let frac = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as i64;
    (secs - NTP_UNIX_EPOCH_DELTA) * 1000 + ((frac * 1000) >> 32)
}

// --- Gecikme ölçümü ----

/// Senkron işlem süresi ölçümü
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed())
}

/// REST gecikmesi: /info {"type":"meta"} round-trip süresi
pub async fn rest_rtt(api_url: &str) -> anyhow::Result<Duration> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let t = Instant::now();
    client
        .post(api_url)
        .json(&json!({ "type": "meta" }))
        .send()
        .await?;
    Ok(t.elapsed())
}
