//! MARKET CTX — canlı bağlam: funding / OI / mark / oracle serileri.
//!
//! Görevler:
//! 1. `apply_event / apply_stream / apply_rest_snapshot`: WS + REST
//!    verisini tek `CtxState` haritasında birleştirir.
//! 2. Türetilen seriler: `delta_oi` (OI değişimi — likidasyon kadransı
//!    girdisi), `oi_series`, `funding_series`, `basis` (perp-spot sapması).
//! Ölçek notu: HL API `funding` alanı **saatlik** orandır (fundingHistory
//! 1h aralıklı; yıllıklaştırma ×24×365) — burada dönüşüm YAPILMAZ, ham
//! değer taşınır (dönüşüm strateji katmanındadır).
use std::collections::{HashMap, VecDeque};


use crate::ingestor::StreamEvent;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CtxState {
    pub coin: String,
    pub funding: f64,
    pub open_interest: f64,
    pub mark_px: f64,
    pub oracle_px: f64,
    pub premium: f64,
    pub day_ntl_vlm: f64,
    /// dünkü kapanış (kesit momentum: mark/prevDay - 1)
    pub prev_day_px: f64,
    pub ts: i64,
}

#[derive(Debug, Clone)]
pub struct MarketCtx {
    ctx_history_len: usize,
    /// coin -> en güncel ctx
    latest: HashMap<String, CtxState>,
    /// coin -> OI serisi (ts, oi) — ΔOI hesabı (strateji 09)
    oi_history: HashMap<String, VecDeque<(i64, f64)>>,
    /// coin -> funding serisi (ts, funding)
    funding_history: HashMap<String, VecDeque<(i64, f64)>>,
}

impl MarketCtx {
    pub fn new(ctx_history_len: usize) -> Self {
        Self {
            ctx_history_len,
            latest: HashMap::new(),
            oi_history: HashMap::new(),
            funding_history: HashMap::new(),
        }
    }

    pub fn apply_event(&mut self, c: CtxState) {
        let coin = c.coin.clone();
        self.oi_history.entry(coin.clone()).or_default().push_back((c.ts, c.open_interest));
        self.funding_history.entry(coin.clone()).or_default().push_back((c.ts, c.funding));
        if let Some(h) = self.oi_history.get_mut(&coin) {
            while h.len() > self.ctx_history_len { h.pop_front(); }
        }
        if let Some(h) = self.funding_history.get_mut(&coin) {
            while h.len() > self.ctx_history_len { h.pop_front(); }
        }
        self.latest.insert(coin, c);
    }

    /// WS StreamEvent::Ctx'yi uygula (diğer event türleri yok sayılır)
    pub fn apply_stream(&mut self, ev: &StreamEvent) {
        if let StreamEvent::Ctx { coin, funding, open_interest, mark_px, oracle_px, premium, day_ntl_vlm, prev_day_px, ts } = ev {
            self.apply_event(CtxState {
                coin: coin.clone(), funding: *funding, open_interest: *open_interest,
                mark_px: *mark_px, oracle_px: *oracle_px, premium: *premium,
                day_ntl_vlm: *day_ntl_vlm, prev_day_px: *prev_day_px, ts: *ts,
            });
        }
    }

    /// REST metaAndAssetCtxs tam taraması — ctx index = universe sırası
    pub fn apply_rest_snapshot(
        &mut self,
        universe: &[String],
        ctxs: &[hyperliquid_api::types::AssetCtx],
    ) -> usize {
        let now = hyperliquid_api::now_ms() as i64;
        let mut n = 0;
        for (i, coin) in universe.iter().enumerate() {
            let c = match ctxs.get(i) {
                Some(c) => c,
                None => continue,
            };
            let st = CtxState {
                coin: coin.clone(),
                funding: c.funding.parse().unwrap_or(0.0),
                open_interest: c.openInterest.parse().unwrap_or(0.0),
                mark_px: c.markPx.parse().unwrap_or(0.0),
                oracle_px: c.oraclePx.parse().unwrap_or(0.0),
                premium: c.premium.clone().and_then(|p| p.parse().ok()).unwrap_or(0.0),
                day_ntl_vlm: c.dayNtlVlm.parse().unwrap_or(0.0),
                prev_day_px: c.prevDayPx.clone().and_then(|p| p.parse().ok()).unwrap_or(0.0),
                ts: now,
            };
            self.apply_event(st);
            n += 1;
        }
        n
    }

    pub fn get(&self, coin: &str) -> Option<&CtxState> {
        self.latest.get(coin)
    }

    pub fn all(&self) -> impl Iterator<Item = &CtxState> {
        self.latest.values()
    }

    /// ΔOI: son iki ölçüm arasındaki open interest değişimi (strateji 09)
    pub fn delta_oi(&self, coin: &str) -> Option<f64> {
        let h = self.oi_history.get(coin)?;
        if h.len() < 2 {
            return None;
        }
        let (t0, oi0) = h[h.len() - 2];
        let (t1, oi1) = h[h.len() - 1];
        if t1 <= t0 {
            return None;
        }
        Some(oi1 - oi0)
    }

    /// Son N ölçümün OI serisi
    pub fn oi_series(&self, coin: &str) -> Vec<(i64, f64)> {
        self.oi_history.get(coin).map(|d| d.iter().copied().collect()).unwrap_or_default()
    }

    /// Funding serisi
    pub fn funding_series(&self, coin: &str) -> Vec<(i64, f64)> {
        self.funding_history.get(coin).map(|d| d.iter().copied().collect()).unwrap_or_default()
    }

    /// Spot-perp basis (strateji 08): (perp mark - spot mid) / spot mid
    /// spot mid allMids'te `@<index>` anahtarıyla gelir.
    pub fn basis(&self, coin: &str, spot_mids: &HashMap<String, f64>, spot_key: &str) -> Option<f64> {
        let mark = self.get(coin)?.mark_px;
        let spot = spot_mids.get(spot_key)?;
        if *spot <= 0.0 {
            return None;
        }
        Some((mark - spot) / spot)
    }

    pub fn tracked(&self) -> usize {
        self.latest.len()
    }
}
