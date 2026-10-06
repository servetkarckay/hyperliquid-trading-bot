//! İMZA — Hyperliquid L1 action imzası (EIP-712, phantom agent).
//!
//! Görevler:
//! 1. `action_hash` + `phantom_agent_digest`: emir/iptal payload'ını
//! Hyperliquid'in beklediği phantom-agent sözleşmesine bağlar.
//! 2. `sign_l1_action`: `L1Signature { r, s, v }` üretir.
//! 3. `float_to_wire`: fiyatları borsanın wire formatına yuvarlar.
//! 4. `HyperliquidClient`: info/exchange action POST'ları; `ExchangeOutcome`
//! SUCCESS / ERROR / **UNKNOWN** (ağ zaman aşımı — borsada işlenmiş
//! OLABİLİR) ayrımını yapar; UNKNOWN yukarıya reconciliation için taşınır.
//! 5. `now_ms`: NTP-offset'li zaman damgası.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use k256::ecdsa::{RecoveryId, SigningKey, VerifyingKey};
use serde_json::{json, Value};
use tiny_keccak::{Hasher, Keccak};

use crate::config::Config;
use crate::error_responses::{ApiError, Result};

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// Nonce **tekil** olmalı. Aynı milisaniyede birden çok imzalı istek atılırsa
/// (paralel kaldıraç ayarı, paralel emir) borsa "duplicate nonce" ile reddeder.
/// Atomik sayaç: max(now_ms, son + 1) — geriye doğru gitmez, çakışmaz.
fn next_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let mut n = now_ms();
    loop {
        let last = LAST.load(Ordering::Relaxed);
        if n <= last {
            n = last + 1;
        }
        match LAST.compare_exchange_weak(last, n, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return n,
            Err(_) => continue,
        }
    }
}

pub fn signing_key_from_hex(pk: &str) -> Result<SigningKey> {
    let bytes = hex::decode(pk.trim().trim_start_matches("0x").to_ascii_lowercase())?;
    if bytes.len() != 32 {
        return Err(ApiError::Sign("private key 32 byte olmali".into()));
    }
    SigningKey::from_slice(&bytes).map_err(|e| ApiError::Sign(format!("gecersiz private key: {e}")))
}

/// Agent (API wallet) adresi: keccak(pubkey)[12..]
pub fn address_of(sk: &SigningKey) -> String {
    let point = VerifyingKey::from(sk).to_encoded_point(false);
    let raw = point.as_bytes(); // 0x04 || X || Y
    let h = keccak256(&raw[1..]);
    hex::encode(&h[12..])
}

/// Keccak256 hash fonksiyonu.
pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(bytes);
    k.finalize(&mut out);
    out
}

/// SDK `float_to_wire` ile birebir: 8 hane, sondaki sifirlar atilir ("29792.0" -> "29792")
pub fn float_to_wire(x: f64) -> String {
    let mut s = format!("{x:.8}");
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if s == "-0" {
        s = "0".into();
    }
    s
}

/// SDK `action_hash`: msgpack(action) + nonce(8B BE) + vault prefixi + expiresAfter flagi
pub fn action_hash(action: &Value, vault_address: Option<&str>, nonce: u64, expires_after: Option<u64>) -> Result<[u8; 32]> {
    let mut data =
        rmp_serde::to_vec(action).map_err(|e| ApiError::Sign(format!("msgpack: {e}")))?;
    data.extend_from_slice(&nonce.to_be_bytes());
    match vault_address {
        None => data.push(0x00),
        Some(v) => {
            let addr = hex::decode(v.trim_start_matches("0x").to_ascii_lowercase())?;
            if addr.len() != 20 {
                return Err(ApiError::Sign("vault adres 20 byte olmali".into()));
            }
            data.push(0x01);
            data.extend_from_slice(&addr);
        }
    }
    match expires_after {
        None => {}
        Some(e) => {
            data.push(0x00);
            data.extend_from_slice(&e.to_be_bytes());
        }
    }
    Ok(keccak256(&data))
}

fn u256_be(n: u64) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[24..].copy_from_slice(&n.to_be_bytes());
    b
}

/// Phantom Agent EIP-712 digest: domain name "Exchange", version 1, chainId 1337
pub fn phantom_agent_digest(connection_id: &[u8; 32], source: &str) -> [u8; 32] {
    let mut d = Vec::with_capacity(160);
    d.extend_from_slice(&keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    ));
    d.extend_from_slice(&keccak256(b"Exchange"));
    d.extend_from_slice(&keccak256(b"1"));
    d.extend_from_slice(&u256_be(1337));
    d.extend_from_slice(&u256_be(0));
    let domain_sep = keccak256(&d);

    let mut s = Vec::with_capacity(96);
    s.extend_from_slice(&keccak256(b"Agent(string source,bytes32 connectionId)"));
    s.extend_from_slice(&keccak256(source.as_bytes()));
    s.extend_from_slice(connection_id);
    let struct_hash = keccak256(&s);

    let mut msg = Vec::with_capacity(70);
    msg.extend_from_slice(&[0x19u8, 0x01]);
    msg.extend_from_slice(&domain_sep);
    msg.extend_from_slice(&struct_hash);
    keccak256(&msg)
}

#[derive(Debug, Clone)]
pub struct L1Signature {
    pub r: String,
    pub s: String,
    pub v: u8,
}

/// SDK `sign_l1_action` ile birebir: RFC6979 deterministik, low-s normalize, v = 27 + recid
pub fn sign_l1_action(
    sk: &SigningKey,
    action: &Value,
    nonce: u64,
    is_mainnet: bool,
    vault: Option<&str>,
    expires_after: Option<u64>,
) -> Result<L1Signature> {
    let hash = action_hash(action, vault, nonce, expires_after)?;
    let source = if is_mainnet { "a" } else { "b" };
    let digest = phantom_agent_digest(&hash, source);

 // RFC6979 deterministik imza + recovery id (SDK ile birebir)
    let (sig, mut recid) = sk
        .sign_prehash_recoverable(&digest)
        .map_err(|e| ApiError::Sign(format!("imza: {e}")))?;
 // EVM low-s: s yukaridaysa normallestir ve parity'yi cevir
    if sig.normalize_s().is_some() {
        recid = RecoveryId::new(!recid.is_y_odd(), recid.is_x_reduced());
    }
 // recovery dogrulamasi (parity tutmazsa bir kez daha cevir)
    let vk = VerifyingKey::from(sk);
    if VerifyingKey::recover_from_prehash(&digest, &sig, recid).ok().as_ref() != Some(&vk) {
        recid = RecoveryId::new(!recid.is_y_odd(), recid.is_x_reduced());
    }
    if VerifyingKey::recover_from_prehash(&digest, &sig, recid).ok().as_ref() != Some(&vk) {
        return Err(ApiError::Sign("recovery dogrulamasi basarisiz".into()));
    }
    Ok(L1Signature {
        r: format!("0x{}", hex::encode(sig.r().to_bytes())),
        s: format!("0x{}", hex::encode(sig.s().to_bytes())),
        v: 27 + u8::from(recid),
    })
}

#[derive(Clone)]
pub struct HyperliquidClient {
    pub cfg: Config,
    pub sk: SigningKey,
    http: reqwest::Client,
}

impl HyperliquidClient {
    pub fn new(cfg: Config) -> Result<Self> {
        Self::with_timeout(cfg, Duration::from_secs(30))
    }

 /// Testnet borsa 30 sn içinde dönmüyorse UNKNOWN şart: transport hatasını
 /// retry etmek blind retry'dir (emir borsada işlenmiş olabilir).
 /// Timeout ayarı dışarıdan verilebilir (testler kısa timeout kullanır).
    pub fn with_timeout(cfg: Config, timeout: Duration) -> Result<Self> {
        let sk = signing_key_from_hex(&cfg.pk_hex())?;
        let http = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { cfg, sk, http })
    }

    pub fn is_mainnet(&self) -> bool {
        self.cfg.network.eq_ignore_ascii_case("mainnet")
    }

 /// Imzalayan agent'in (API wallet) adresi
    pub fn agent_address(&self) -> String {
        address_of(&self.sk)
    }

 /// POST /info için: 429/5xx retry ile (idempotent okumalar)
    async fn post_info(&self, body: &Value) -> Result<Value> {
        const MAX_TRY: u32 = 5;
        let mut attempt = 0u32;
        loop {
            match self.post_once("/info", body).await {
                Ok(v) => return Ok(v),
                Err(ApiError::Http { status, body }) if (status == 429 || status >= 500) && attempt < MAX_TRY => {
                    let wait = 500u64 * 2u64.pow(attempt);
                    tracing::warn!(
                        "HTTP {status} ({}), {wait} ms bekleyip tekrar ({}/{})",
                        body.chars().take(80).collect::<String>(),
                        attempt + 1, MAX_TRY
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

 /// POST /exchange için: 429 -> retry; 5xx/timeout -> UNKNOWN (retry YOK).
 /// Cagiran taraf UNKNOWN alip reconciliation yapar (refresh_orders + cloid eslesme).
    async fn post_exchange(&self, action: &Value) -> ExchangeOutcome {
        const MAX_TRY_429: u32 = 5;
        let mut attempt_429 = 0u32;
        loop {
            let nonce = next_nonce();
            let sig = match sign_l1_action(&self.sk, action, nonce, self.is_mainnet(), None, None) {
                Ok(s) => s,
                Err(e) => return ExchangeOutcome::Error(e),
            };
            let body = json!({
                "action": action,
                "nonce": nonce,
                "signature": { "r": sig.r, "s": sig.s, "v": sig.v },
                "vaultAddress": null,
                "expiresAfter": null,
            });
            match self.post_once("/exchange", &body).await {
                Ok(v) => return ExchangeOutcome::Success(v),
                Err(ApiError::Http { status, body }) if status == 429 && attempt_429 < MAX_TRY_429 => {
                    let wait = 500u64 * 2u64.pow(attempt_429);
                    tracing::warn!(
                        "HTTP 429 ({}), {wait} ms bekleyip tekrar ({}/{})",
                        body.chars().take(80).collect::<String>(),
                        attempt_429 + 1, MAX_TRY_429
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    attempt_429 += 1;
                }
                Err(ApiError::Http { status, .. }) if status >= 500 => {
 // 5xx -> retry YOK, UNKNOWN dondur
                    tracing::error!("[EXCHANGE] HTTP {status} -- UNKNOWN (retry YOK, reconciliation bekleniyor)");
                    return ExchangeOutcome::Unknown;
                }
                Err(ApiError::Http { status: 0, body }) => {
 // HATA 1: reqwest transport hatası (timeout, connect
 // kopması, gövde okunamadı) `From<reqwest::Error>` ile
 // `Http { status: 0 }` olur. `status >= 500` bu yüzden
 // YAKALAMAZ ve emir "Error" sayılırdı. Oysa istek sunucuya
 // ULAŞMIŞ olabilir -> sonuç UNKNOWN'dır.
                    tracing::error!("[EXCHANGE] transport ({body}) -- UNKNOWN (timeout dahil, retry YOK)");
                    return ExchangeOutcome::Unknown;
                }
                Err(e) => return ExchangeOutcome::Error(e),
            }
        }
    }

 /// REST info sorgusu: POST /info (429/5xx retry ile)
    pub async fn info(&self, body: Value) -> Result<Value> {
        self.post_info(&body).await
    }

 /// Imzali aksiyon: POST /exchange -- UNKNOWN/Success/Error dondurur.
 /// 429 -> retry; 5xx/timeout -> UNKNOWN (retry YOK).
    pub async fn exchange_action(&self, action: Value) -> ExchangeOutcome {
        self.post_exchange(&action).await
    }

 /// Tek seferlik HTTP POST (retry mantığı yok).
    async fn post_once(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.cfg.api_url, path);
        let resp = self.http.post(&url).json(body).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status != 200 {
            return Err(ApiError::Http { status, body: text });
        }
        let v: Value = serde_json::from_str(&text)?;
        if let Some(err) = v.get("error").and_then(Value::as_str) {
            return Err(ApiError::Api(err.to_string()));
        }
        if v.get("status").and_then(Value::as_str) == Some("err") {
            let msg = v.get("response").and_then(Value::as_str).unwrap_or("bilinmeyen hata");
            return Err(ApiError::Api(msg.to_string()));
        }
        Ok(v)
    }
}

// Exchange sonucu -- Success / Unknown (5xx/timeout) / Error
#[derive(Debug)]
pub enum ExchangeOutcome {
    Success(Value),
    Unknown,
    Error(ApiError),
}

#[cfg(test)]
mod exchange_unknown_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

 /// Sahte borsa: her isteği sayar, verilen yanıtları sırayla döner.
 /// `None` = isteği AL but YANITLAMA (istemci timeout'a düşer -> UNKNOWN).
    struct Fake {
        url: String,
        hits: Arc<AtomicUsize>,
    }

    async fn fake_exchange(responses: Vec<Option<(&'static str, &'static str)>>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let h2 = hits.clone();
        tokio::spawn(async move {
            let mut i = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
 // isteği oku (Content-Length kadar) — reqwest tam göndermeden
 // yanıt vermek bağlantı reseti yapar, sayım kaçar.
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                h2.fetch_add(1, Ordering::SeqCst);
                let r = responses
                    .get(i.min(responses.len().saturating_sub(1)))
                    .copied()
                    .flatten();
                i += 1;
                let Some((status, body)) = r else {
 // YANIT VERME: timeout'u taklit et
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                };
                if n == 0 {
                    continue;
                }
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        Fake { url: format!("http://{addr}"), hits }
    }

    fn cfg(url: &str) -> Config {
        Config {
            network: "testnet".into(),
            api_url: url.into(),
            ws_url: "ws://127.0.0.1:1".into(),
 // testnet gerçek anahtarı değil, geçerli bir test ağı anahtarı
            private_key: "0x1111111111111111111111111111111111111111111111111111111111111111".into(),
            agent_address: "0x".into(),
        }
    }

    fn action() -> Value {
        json!({"type": "order", "orders": [], "grouping": "na"})
    }

 /// 5xx -> UNKNOWN, retry YOK (ikinci POST olmamalı).
    #[tokio::test]
    async fn exchange_5xx_kor_retry_yok() {
        let f = fake_exchange(vec![Some(("500 Internal Server Error", r#"{"error":"boom"}"#))]).await;
        let c = HyperliquidClient::with_timeout(cfg(&f.url), Duration::from_secs(3)).unwrap();
        let out = c.exchange_action(action()).await;
        assert!(
            matches!(out, ExchangeOutcome::Unknown),
            "5xx UNKNOWN olmali, got {out:?}"
        );
 // retry YOK: ikinci POST gönderilmemeli
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(f.hits.load(Ordering::SeqCst), 1, "5xx sonrasi ikinci POST VAR — blind retry!");
    }

 /// 429 -> backoff ile retry, ikinci denemede başarı.
    #[tokio::test]
    async fn exchange_429_backoff_ile_retry() {
        let ok = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":42}}]}}}"#;
        let f = fake_exchange(vec![
            Some(("429 Too Many Requests", r#"{"error":"rate limit"}"#)),
            Some(("200 OK", ok)),
        ])
        .await;
        let c = HyperliquidClient::with_timeout(cfg(&f.url), Duration::from_secs(5)).unwrap();
        let out = c.exchange_action(action()).await;
        match out {
            ExchangeOutcome::Success(v) => {
                assert_eq!(
                    v.pointer("/response/data/statuses/0/resting/oid").and_then(Value::as_i64),
                    Some(42),
                    "başarılı yanıt parse edilemedi"
                );
            }
            other => panic!("429 sonrası başarı bekleniyordu, got {other:?}"),
        }
        assert_eq!(f.hits.load(Ordering::SeqCst), 2, "429 -> tam 1 retry bekleniyordu");
    }

 /// timeout -> UNKNOWN, retry YOK.
 ///
 /// `reqwest` timeout'u `ApiError::Http { status: 0 }` olur; `status >= 500`
 /// bunu YAKALAMAZ. ÖNCE bu durum `Error` oluyordu (bilgi kaybı).
    #[tokio::test]
    async fn exchange_timeout_unknown_retry_yok() {
        let f = fake_exchange(vec![None]).await; // hiç yanıt verme -> timeout
        let c = HyperliquidClient::with_timeout(cfg(&f.url), Duration::from_millis(400)).unwrap();
        let out = c.exchange_action(action()).await;
        assert!(
            matches!(out, ExchangeOutcome::Unknown),
            "timeout UNKNOWN olmali, got {out:?}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(f.hits.load(Ordering::SeqCst), 1, "timeout sonrasi ikinci POST VAR!");
    }

 /// Timeout UNKNOWN'yu verir, ama 4xx (emir gerçekten gitmedi) DEĞİL —
 /// 4xx normal hata olarak kalmalı, reconciliation'a GÖNDERİLMEMELİ.
    #[tokio::test]
    async fn exchange_4xx_hata_kalir_unknown_degil() {
        let f = fake_exchange(vec![Some(("422 Unprocessable Entity", r#"{"error":"invalid price"}"#))]).await;
        let c = HyperliquidClient::with_timeout(cfg(&f.url), Duration::from_secs(3)).unwrap();
        let out = c.exchange_action(action()).await;
        assert!(
            matches!(out, ExchangeOutcome::Error(_)),
            "4xx Error olmali (emir borsaya hic gitmedi), got {out:?}"
        );
    }

 /// 4xx ASLA retry edilmez (mevcut davranış, regresyon koruması).
    #[tokio::test]
    async fn exchange_4xx_retry_yok() {
        let f = fake_exchange(vec![Some(("400 Bad Request", r#"{"error":"bad"}"#))]).await;
        let c = HyperliquidClient::with_timeout(cfg(&f.url), Duration::from_secs(3)).unwrap();
        let _ = c.exchange_action(action()).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(f.hits.load(Ordering::SeqCst), 1, "4xx retry edilmemeli");
    }
}
