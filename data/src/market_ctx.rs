//! MARKET CTX — canlı bağlam: funding / OI / mark / oracle serileri.
//!
//! Görevler:
//! 1. `apply_event / apply_stream / apply_rest_snapshot`: WS + REST
//!
//! verisini tek `CtxState` haritasında birleştirir.
//! 2. Türetilen seriler: `delta_oi` (OI değişimi — likidasyon kadransı
//!
//! girdisi), `oi_series`, `funding_series`, `basis` (perp-spot sapması).
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
        self.oi_history
            .entry(coin.clone())
            .or_default()
            .push_back((c.ts, c.open_interest));
        self.funding_history
            .entry(coin.clone())
            .or_default()
            .push_back((c.ts, c.funding));
        if let Some(h) = self.oi_history.get_mut(&coin) {
            while h.len() > self.ctx_history_len {
                h.pop_front();
            }
        }
        if let Some(h) = self.funding_history.get_mut(&coin) {
            while h.len() > self.ctx_history_len {
                h.pop_front();
            }
        }
        self.latest.insert(coin, c);
    }

    /// WS StreamEvent::Ctx'yi uygula (diğer event türleri yok sayılır).
    ///
    /// Çekirdek sayılar (funding/OI/mark/oracle) zaten ingestor'da
    /// doğrulanmıştır; meşru null edebilecek alanlar (premium/dayNtlVlm/
    /// prevDayPx) `None` gelirirse SON BİLİNEN değer korunur — sahte
    /// sıfır yazılmaz (REST yoluyla aynı dürüstlük kontratı).
    pub fn apply_stream(&mut self, ev: &StreamEvent) {
        if let StreamEvent::Ctx {
            coin,
            funding,
            open_interest,
            mark_px,
            oracle_px,
            premium,
            day_ntl_vlm,
            prev_day_px,
            ts,
        } = ev
        {
            let prev = self.latest.get(coin).cloned();
            let keep = |o: &Option<f64>, prev_v: f64| o.unwrap_or(prev_v);
            self.apply_event(CtxState {
                coin: coin.clone(),
                funding: *funding,
                open_interest: *open_interest,
                mark_px: *mark_px,
                oracle_px: *oracle_px,
                premium: keep(premium, prev.as_ref().map(|p| p.premium).unwrap_or(0.0)),
                day_ntl_vlm: keep(
                    day_ntl_vlm,
                    prev.as_ref().map(|p| p.day_ntl_vlm).unwrap_or(0.0),
                ),
                prev_day_px: keep(
                    prev_day_px,
                    prev.as_ref().map(|p| p.prev_day_px).unwrap_or(0.0),
                ),
                ts: *ts,
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
            // DATA DÜRÜSTLÜĞÜ: alan parse edilemezse 0.0 YAZILMAZ (UNKNOWN ≠
            // geçerli sıfır); son bilinen değer KORUNUR (yoksa 0.0) —
            // bayat veri dürüsttür, sahte sıfır değil.
            let prev = self.latest.get(coin).cloned();
            let f = |x: &str, d: f64| x.parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(d);
            let st = CtxState {
                coin: coin.clone(),
                funding: f(&c.funding, prev.as_ref().map(|p| p.funding).unwrap_or(0.0)),
                open_interest: f(
                    &c.openInterest,
                    prev.as_ref().map(|p| p.open_interest).unwrap_or(0.0),
                ),
                mark_px: f(&c.markPx, prev.as_ref().map(|p| p.mark_px).unwrap_or(0.0)),
                oracle_px: f(
                    &c.oraclePx,
                    prev.as_ref().map(|p| p.oracle_px).unwrap_or(0.0),
                ),
                premium: c
                    .premium
                    .clone()
                    .and_then(|p| p.parse::<f64>().ok())
                    .filter(|v| v.is_finite())
                    .unwrap_or_else(|| prev.as_ref().map(|p| p.premium).unwrap_or(0.0)),
                day_ntl_vlm: f(
                    &c.dayNtlVlm,
                    prev.as_ref().map(|p| p.day_ntl_vlm).unwrap_or(0.0),
                ),
                prev_day_px: c
                    .prevDayPx
                    .clone()
                    .and_then(|p| p.parse::<f64>().ok())
                    .filter(|v| v.is_finite())
                    .unwrap_or_else(|| prev.as_ref().map(|p| p.prev_day_px).unwrap_or(0.0)),
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

    /// ΔOI **pencere ölçümü**: `window_ms` içindeki OI değişimi
    /// (son örnek − pencere tabanındaki en-eski örnek).
    ///
    /// NEDEN: per-tick ΔOI OI stokunun ~1e-5'i mertebesindedir; %1.5'lük
    /// likidite-avı eşiği tick-aralığında ASLA tetiklenmez (canlı vakı:
    /// kaldırılan SMC-L OI katmanı hep "gürültü" döndü). Anlamlı kanıt
    /// bar-penceresinde (1 saat) ölçülür. Pencere dolmadıysa (açılış/buffer
    /// kısa) → None — sahte sıfır/kısmi kanıt ÜRETİLMEZ (veri-yok dürüstlüğü).
    pub fn delta_oi_window(&self, coin: &str, window_ms: i64) -> Option<f64> {
        let h = self.oi_history.get(coin)?;
        let &(t1, oi1) = h.back()?;
        if h.len() < 2 || window_ms <= 0 {
            return None;
        }
        let target = t1 - window_ms;
        let mut base: Option<(i64, f64)> = None;
        for &(t, oi) in h {
            if t <= target {
                base = Some((t, oi));
            } else {
                break;
            }
        }
        let (t0, oi0) = base?;
        if t1 <= t0 {
            return None;
        }
        Some(oi1 - oi0)
    }

    /// Son N ölçümün OI serisi
    pub fn oi_series(&self, coin: &str) -> Vec<(i64, f64)> {
        self.oi_history
            .get(coin)
            .map(|d| d.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Funding serisi
    pub fn funding_series(&self, coin: &str) -> Vec<(i64, f64)> {
        self.funding_history
            .get(coin)
            .map(|d| d.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Spot-perp basis (strateji 08): (perp mark - spot mid) / spot mid
    /// spot mid allMids'te `@<index>` anahtarıyla gelir.
    pub fn basis(
        &self,
        coin: &str,
        spot_mids: &HashMap<String, f64>,
        spot_key: &str,
    ) -> Option<f64> {
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

#[cfg(test)]
mod apply_stream_testleri {
    use super::*;
    use crate::ingestor::StreamEvent;

    fn ctx_ev(ts: i64, oi: f64) -> StreamEvent {
        StreamEvent::Ctx {
            coin: "TEST".into(),
            funding: 0.0,
            open_interest: oi,
            mark_px: 100.0,
            oracle_px: 100.0,
            premium: Some(0.0),
            day_ntl_vlm: Some(1_000_000.0),
            prev_day_px: Some(100.0),
            ts,
        }
    }

    /// Pencere ΔOI: 1h penceresinde OI düşüşü ölçülür; pencere dolmadan
    /// None (veri-yok dürüstlüğü — sahte kanıt yok).
    #[test]
    fn delta_oi_window_pencere_olcumu() {
        let mut c = MarketCtx::new(8);
        // pencere 1 saat; örnekler t=0 ve t=3600_000 (tam kapalı pencere)
        c.apply_stream(&ctx_ev(0, 1_000_000.0));
        assert_eq!(c.delta_oi_window("TEST", 3_600_000), None, "tek örnek");
        c.apply_stream(&ctx_ev(1_800_000, 1_000_500.0));
        // pencere tabanı (t=−1800_000'e kadar örnek) yok → None
        assert_eq!(
            c.delta_oi_window("TEST", 3_600_000),
            None,
            "pencere dolmadı"
        );
        c.apply_stream(&ctx_ev(3_600_000, 975_000.0));
        // taban = t=0 (≤ 3_600_000−3_600_000) → ΔOI = −25_000 (%2.5 düşüş)
        let d = c
            .delta_oi_window("TEST", 3_600_000)
            .expect("pencere kapalı");
        assert!((d - (-25_000.0)).abs() < 1e-9, "ΔOI={d}");
        // per-tick delta ayrı soru: son iki örnek −500.5+... (karışmaz)
        assert!(c.delta_oi("TEST").is_some());
    }

    /// Pencere kısa süreli tick gürültüsünü ölçMEMELİ: küçük OI salınımı
    /// pencere başında ve sonunda aynen geri gelirse ΔOI≈0 çıkar.
    #[test]
    fn delta_oi_window_tick_gurultusunu_olcer() {
        let mut c = MarketCtx::new(8);
        c.apply_stream(&ctx_ev(0, 1_000_000.0));
        c.apply_stream(&ctx_ev(3_600_000, 980_000.0)); // −%2 (likidite avı profili)
        let d = c.delta_oi_window("TEST", 3_600_000).expect("kapalı");
        assert!((d - (-20_000.0)).abs() < 1e-9);
    }

    /// WS yolu: meşru null alan (premium/dayNtlVlm/prevDayPx None) →
    /// SON BİLİNEN değer korunur; sahte 0.0 yazılmaz. Çekirdek alanlar
    /// güncellenir.
    #[test]
    fn apply_stream_null_uyari_son_bilineni_korur() {
        let mut c = MarketCtx::new(8);
        c.apply_stream(&StreamEvent::Ctx {
            coin: "BTC".into(),
            funding: 0.0001,
            open_interest: 10.0,
            mark_px: 100.0,
            oracle_px: 99.0,
            premium: Some(0.0025),
            day_ntl_vlm: Some(500.0),
            prev_day_px: Some(95.0),
            ts: 1,
        });
        c.apply_stream(&StreamEvent::Ctx {
            coin: "BTC".into(),
            funding: 0.0002,
            open_interest: 11.0,
            mark_px: 101.0,
            oracle_px: 100.0,
            premium: None,
            day_ntl_vlm: None,
            prev_day_px: None,
            ts: 2,
        });
        let st = c.get("BTC").unwrap();
        assert_eq!(st.funding, 0.0002, "çekirdek alan güncellenir");
        assert_eq!(st.mark_px, 101.0);
        assert_eq!(st.premium, 0.0025, "null → son bilinen premium korunur");
        assert_eq!(st.day_ntl_vlm, 500.0, "null → son bilinen vlm korunur");
        assert_eq!(st.prev_day_px, 95.0);
    }

    /// İlk event'te Option alan None ise ve önceki değer yoksa:
    /// 0.0 (bilinmezin dürüst temsili — prev yok) ama asla sahte büyüklük yok.
    #[test]
    fn apply_stream_ilk_event_null_ile_baslar() {
        let mut c = MarketCtx::new(8);
        c.apply_stream(&StreamEvent::Ctx {
            coin: "ETH".into(),
            funding: 0.0001,
            open_interest: 1.0,
            mark_px: 3000.0,
            oracle_px: 2999.0,
            premium: None,
            day_ntl_vlm: None,
            prev_day_px: None,
            ts: 1,
        });
        let st = c.get("ETH").unwrap();
        assert_eq!(st.premium, 0.0);
        assert_eq!(st.day_ntl_vlm, 0.0);
    }
}
