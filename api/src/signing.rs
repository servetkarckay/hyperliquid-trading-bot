//! İmzalama ve HTTP taşıma — resmi api/signing.md + nonces-and-api-wallets.md.
//!
//! Hyperliquid imza zinciri (SDK 0.24 ile bayt-bayt doğrulanmış):
//! 1. Action (JSON, alan sırası korunur) -> msgpack serileştirme
//! 2. + nonce (8B big-endian, unix ms) + vault prefixi (0x00/0x01+20B)
//! 3. + expiresAfter (varsa 0x00 + 8B BE)
//! 4. keccak256 -> action hash (connectionId)
//! 5. Phantom Agent EIP-712: domain name="Exchange", version="1",
//!    chainId=1337, verifyingContract=0x0; primary "Agent(string source,
//!    bytes32 connectionId)"; source="a" (mainnet) / "b" (testnet)
//! 6. secp256k1 RFC6979 deterministik imza, low-s normalize, v=27+recoveryId
//!
//! NONCE: unix ms; (T-2gün, T+1gün) penceresinde olmalı, imzalayan başına
//! (API wallet = agent adresi) takip edilir. `now_ms()` üretir.
//!
//! Ayrıca bu dosya HTTP taşımayı içerir: POST /info (sorgular) ve
//! POST /exchange (imzalı aksiyonlar; zarf: action+nonce+signature+
//! vaultAddress+expiresAfter). float_to_wire: fiyat/boyut string formatı
//! (8 hane, sondaki sıfırlar atılır — "29792.0" -> "29792").
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

pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    let mut k = Keccak::v256();
    let mut out = [0u8; 32];
    k.update(bytes);
    k.finalize(&mut out);
    out
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
        None => {} // SDK: expires_after yoksa hicbir sey eklenmez
        Some(e) => {
            data.push(0x00); // SDK 0.24: flag 0x00
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
        let sk = signing_key_from_hex(&cfg.pk_hex())?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { cfg, sk, http })
    }

    pub fn is_mainnet(&self) -> bool {
        self.cfg.network.eq_ignore_ascii_case("mainnet")
    }

    /// Imzalayan agent'in (API wallet) adresi
    pub fn agent_address(&self) -> String {
        address_of(&self.sk)
    }

    /// 429/5xx için üstel geri çekilmeli tekrar. Rate limit (ağırlık/dakika)
    /// 178 coin'lik evrende backfill sırasında rutin olarak tetikleniyor ve
    /// ham hata **tüm botu öldürüyordu**. 429 borsanın reddi demektir, işlem
    /// yapılmadığından imzalı aksiyonlarda da tekrar güvenlidir.
    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        const MAX_TRY: u32 = 5;
        let mut attempt = 0u32;
        loop {
            match self.post_once(path, body).await {
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

    /// REST info sorgusu: POST /info
    pub async fn info(&self, body: Value) -> Result<Value> {
        self.post("/info", &body).await
    }

    /// Imzali aksiyon: POST /exchange — SDK 0.24 formati
    /// (action tamami "type" dahil imzalanir, nonce = unix ms, vault/expiresAfter null)
    pub async fn exchange_action(&self, action: Value) -> Result<Value> {
        let nonce = next_nonce();
        let sig = sign_l1_action(&self.sk, &action, nonce, self.is_mainnet(), None, None)?;
        let body = json!({
            "action": action,
            "nonce": nonce,
            "signature": { "r": sig.r, "s": sig.s, "v": sig.v },
            "vaultAddress": null,
            "expiresAfter": null,
        });
        self.post("/exchange", &body).await
    }
}
