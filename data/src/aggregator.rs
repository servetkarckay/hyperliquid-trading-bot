//! AGGREGATOR — tick akışını çoklu interval (1m..1d) mumlara çevirir.
//!
//! Görevler:
//! 1. `on_trade`: her tick'te açık mumu günceller; bucket kapanınca
//!    mumu `closed=true` olarak yayınlar.
//! 2. `current` / `history`: canlı (kapanmamış) mum ve kapanmış mum
//!    penceresi — motor stratejilere kapanmış geçmiş + canlı son bar verir.
//! 3. `taker_balance`: TAKER_WINDOW_SECS (300 sn) penceresinde alış/satış
//!    hacim dengesi.
//! 4. `is_stale`: donmuş veri sinyale karışmasın (stale-skip kapısı).
use std::collections::{HashMap, VecDeque};

use anyhow::Result;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Candle {
    /// bucket açılışı (unix ms)
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
    pub n: u64,
    /// tamamlanmış mı (false: canlı mum)
    pub closed: bool,
}

#[derive(Debug, Clone)]
pub struct AggregatorConfig {
    /// interval'lar (saniye). Varsayılan: 1m, 5m, 15m, 1h
    pub intervals_secs: Vec<u64>,
    /// coin+interval başına tutulacak kapanmış mum sayısı (ALT SINIR)
    pub history_len: usize,
    /// ZAMAN PENCERESİ (sn). >0 ve `interval >= history_span_min_iv` ise mum
    /// sayısı `history_span_secs / interval` olacak şekilde KORUNUR
    /// (üst sınır `history_len_max`). 0 = kapalı (sadece history_len).
    ///
    /// NEDEN: eğitim 4400 bar (1h) ile yapıldı ama canlıda 500'lük sabit cap
    /// vardı — ilk kapanan canlı mumda `pop_front` 4400'ü 500'e indiriyordu
    /// (Elliott/Dow/GARCH %87'sini görmeden çalışıyor). EĞİTİM/CANLI
    /// UYUŞMAZLIĞI.
    pub history_span_secs: u64,
    /// Zaman penceresi hangi interval'dan itibaren uygulanır (alt sınır).
    /// Kısa interval'ler (1m/5m/15m) mikro-yapı stratejilerine ait ve kısa
    /// bellekle yetinir; 4400'lük 1m mumu (3 gün) gereksiz bellek yerdi.
    pub history_span_min_iv: u64,
    /// Zaman penceresinden gelen üst sınır (bellek koruması).
    pub history_len_max: usize,
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        // stratejiler: 1m (MM/lob), 5m/15m (indikator), 1h (momentum), 4h+1d (Elliott/Turtle/Dow)
        Self {
            intervals_secs: vec![60, 300, 900, 3600, 14_400, 86_400],
            history_len: 500,
            history_span_secs: 0,
            history_span_min_iv: 3600,
            history_len_max: 4500,
        }
    }
}

/// Taker akışı penceresi (saniye) — 23-breakout: taker buy/sell dengesi
pub const TAKER_WINDOW_SECS: i64 = 300;

#[derive(Debug, Clone, Copy)]
struct CandleState {
    bucket: i64,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: f64,
    n: u64,
}

#[derive(Default)]
pub struct Aggregator {
    cfg: AggregatorConfig,
    /// (coin, interval) -> canlı mum
    live: HashMap<(String, u64), CandleState>,
    /// (coin, interval) -> kapanmış mum geçmişi (en yeni sonda) — VecDeque untuk push/pop
    history: HashMap<(String, u64), VecDeque<Candle>>,
    /// (coin, interval) -> contiguous snapshot for history() — only updated on candle close
    history_snapshot: HashMap<(String, u64), Vec<Candle>>,
    /// taker akışı: coin -> (ts, sz, is_buy) kayıtları
    taker: HashMap<String, VecDeque<(i64, f64, bool)>>,
    /// oturum VWAP: coin -> (toplam px*sz, toplam sz)
    vwap: HashMap<String, (f64, f64)>,
}

impl Aggregator {
    pub fn new(cfg: AggregatorConfig) -> Self {
        Self { cfg, live: HashMap::new(), history: HashMap::new(), history_snapshot: HashMap::new(), taker: HashMap::new(), vwap: HashMap::new() }
    }

    /// Bir tick'i işler; kapanan mumları döndürür (coin, interval, candle).
    pub fn on_trade(
        &mut self,
        coin: &str,
        px: f64,
        sz: f64,
        ts_ms: i64,
        is_buy: bool,
    ) -> Vec<(String, u64, Candle)> {
        let mut closed = Vec::new();
        if px <= 0.0 || sz < 0.0 {
            return closed;
        }
        // taker akışı (23-breakout: buy/sell dengesi) — pencere dışı kayıtlar atılır
        {
            let t = self.taker.entry(coin.to_string()).or_default();
            t.push_back((ts_ms, sz, is_buy));
            let cutoff = ts_ms - TAKER_WINDOW_SECS * 1000;
            while t.front().is_some_and(|(ts, _, _)| *ts < cutoff) {
                t.pop_front();
            }
        }
        // oturum VWAP
        let v = self.vwap.entry(coin.to_string()).or_default();
        v.0 += px * sz;
        v.1 += sz;
        for &iv in &self.cfg.intervals_secs {
            let iv_ms = (iv * 1000) as i64;
            let bucket = ts_ms.div_euclid(iv_ms) * iv_ms;
            let key = (coin.to_string(), iv);
            // cap borrow çakışmasını önlemek için match'ten ÖNCE hesapla
            let hl = self.cap_for(iv);
            match self.live.get_mut(&key) {
                Some(st) if st.bucket == bucket => {
                    st.h = st.h.max(px);
                    st.l = st.l.min(px);
                    st.c = px;
                    st.v += sz;
                    st.n += 1;
                }
                Some(st) => {
                    // önceki mum kapandı
                    let done = Candle {
                        t: st.bucket, o: st.o, h: st.h, l: st.l, c: st.c,
                        v: st.v, n: st.n, closed: true,
                    };
                    self.history.entry(key.clone()).or_default().push_back(done.clone());
                    let h = self.history.get_mut(&key).unwrap();
                    while h.len() > hl {
                        h.pop_front();
                    }
                    // P1-B: contiguous snapshot güncelle — sadece mum kapanışında (O(1) amortized)
                    self.history_snapshot.insert(key.clone(), h.iter().cloned().collect());
                    closed.push((coin.to_string(), iv, done));
                    *st = CandleState { bucket, o: px, h: px, l: px, c: px, v: sz, n: 1 };
                }
                None => {
                    self.live.insert(key, CandleState { bucket, o: px, h: px, l: px, c: px, v: sz, n: 1 });
                }
            }
        }
        closed
    }

    /// Taker akış dengesi (son 5 dk): (buy_sz, sell_sz, buy_oranı) — 23-breakout
    pub fn taker_balance(&self, coin: &str) -> Option<(f64, f64, f64)> {
        let t = self.taker.get(coin)?;
        let mut buy = 0.0;
        let mut sell = 0.0;
        for (_, sz, b) in t {
            if *b { buy += sz } else { sell += sz }
        }
        if buy + sell <= 0.0 {
            return None;
        }
        Some((buy, sell, buy / (buy + sell)))
    }

    /// Oturum VWAP (tick akışından)
    pub fn vwap(&self, coin: &str) -> Option<f64> {
        let (pv, v) = self.vwap.get(coin)?;
        if *v <= 0.0 { None } else { Some(pv / v) }
    }

    /// Canlı (henüz kapanmamış) mum
    pub fn current(&self, coin: &str, interval: u64) -> Option<Candle> {
        self.live.get(&(coin.to_string(), interval)).map(|s| Candle {
            t: s.bucket, o: s.o, h: s.h, l: s.l, c: s.c, v: s.v, n: s.n, closed: false,
        })
    }

    pub fn history(&self, coin: &str, interval: u64) -> &[Candle] {
        self.history_snapshot
            .get(&(coin.to_string(), interval))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn coins_tracked(&self) -> usize {
        self.live.keys().map(|(c, _)| c).collect::<std::collections::HashSet<_>>().len()
    }

    /// REST candleSnapshot ile geçmiş doldurma (WS başlamadan önce)
    /// Bu interval için tutulacak kapanmış mum sayısı.
    pub fn cap_for(&self, iv: u64) -> usize {
        let base = self.cfg.history_len;
        if self.cfg.history_span_secs == 0 || iv < self.cfg.history_span_min_iv {
            return base;
        }
        let want = (self.cfg.history_span_secs / iv.max(1)) as usize;
        want.clamp(base, self.cfg.history_len_max)
    }

    /// Zaman penceresini aç/kapat (canlı: eğitimle aynı pencere).
    /// `min_iv` = pencerenin uygulanacağı en kısa interval.
    pub fn configure_history(&mut self, span_secs: u64, max_len: usize, min_iv: u64) {
        self.cfg.history_span_secs = span_secs;
        self.cfg.history_len_max = max_len.max(self.cfg.history_len);
        self.cfg.history_span_min_iv = min_iv;
    }

    /// P1-4: STALE-SKIP — coin verisi donmuş mu?
    /// `max_age = 2 × interval_secs` (ör. 1h interval → 2h = 7200s).
    /// Kapanan mum yoksa ve canlı mum da eskiyse → true (sinyal SKIP + [STALE]).
    /// ExitsOnly ile aynı kipte raporlanır: sinyal öğrenmeye karışmaz, pozisyon yönetimi
    /// (SL/TP/trailing) etkilenmez.
    pub fn is_stale(&self, coin: &str, interval_secs: u64, max_age_secs: Option<u64>) -> bool {
        let iv = interval_secs;
        let age = max_age_secs.unwrap_or(iv * 2);
        let now = hyperliquid_api::now_ms() as i64;
        // Kapanmış mum varsa onun KAPANIŞ zamanı (t + interval); yoksa canlı mum bucket'ı
        let key = (coin.to_string(), iv);
        if let Some(hist) = self.history.get(&key) {
            if let Some(last) = hist.back() {
                let last_close_ts = last.t + (iv as i64 * 1000); // mum kapanış zamanı
                return (now - last_close_ts) / 1000 > age as i64;
            }
        }
        // kapanmış yok → canlı mum bucket zamanı (henüz kapanmamış)
        if let Some(live) = self.live.get(&key) {
            let bucket_ts = live.bucket;
            return (now - bucket_ts) / 1000 > age as i64;
        }
        // hiç veri yok → stale say (sinyal Skip)
        true
    }

    pub async fn backfill(
        &mut self,
        info: &hyperliquid_api::Info,
        coin: &str,
        interval_secs: u64,
        bars: usize,
    ) -> Result<usize> {
        let iv_name = match interval_secs {
            60 => "1m", 300 => "5m", 900 => "15m", 3600 => "1h",
            14_400 => "4h", 86_400 => "1d", other => {
                anyhow::bail!("bilinmeyen interval: {other}s")
            }
        };
        let end = hyperliquid_api::now_ms() as i64;
        let start = end - (interval_secs as i64) * 1000 * (bars as i64 + 1);
        let raw = info.candle_snapshot(coin, iv_name, start, end).await?;
        // en yeni `bars` mum, eskiden yeniye push edilir
        let recent: Vec<&hyperliquid_api::types::Candle> =
            raw.iter().rev().take(bars).collect();
        let mut n = 0;
        for c in recent.into_iter().rev() {
            let candle = Candle {
                t: c.t,
                o: c.o.parse().unwrap_or(0.0),
                h: c.h.parse().unwrap_or(0.0),
                l: c.l.parse().unwrap_or(0.0),
                c: c.c.parse().unwrap_or(0.0),
                v: c.v.parse().unwrap_or(0.0),
                n: c.n.unwrap_or(0) as u64,
                closed: true,
            };
            self.history
                .entry((coin.to_string(), interval_secs))
                .or_default()
                .push_back(candle);
            n += 1;
        }
        // backfill de aynı pencereye kırpılır (aksi halde 1. canlı mumda
        // pop_front 4400'ü 500'e indiriyordu -> uzun ufuklu stratejiler kör)
        let hl = self.cap_for(interval_secs);
        if let Some(h) = self.history.get_mut(&(coin.to_string(), interval_secs)) {
            while h.len() > hl {
                h.pop_front();
            }
            // P1-B: contiguous snapshot güncelle
            self.history_snapshot.insert((coin.to_string(), interval_secs), h.iter().cloned().collect());
            return Ok(h.len());
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tek interval'lık test aggregator'ı: `cap` mum tutulur, zaman penceresi kapalı.
    fn agg_single_iv(iv_secs: u64, cap: usize) -> Aggregator {
        Aggregator::new(AggregatorConfig {
            intervals_secs: vec![iv_secs],
            history_len: cap,
            history_span_secs: 0,
            history_span_min_iv: u64::MAX,
            history_len_max: cap,
        })
    }

    /// P1-B kabul: VecDeque wrap olduğunda `history()` TAM ve KRONOLOJİK pencere
    /// döndürmeli.
    ///
    /// ÖNCE: `history()` içinde `as_slices().0` kullanılıyordu; head `start`
    /// sıfırdan uzaklaştığında ikinci fiziksel slice'taki mumlar görünmüyordu —
    /// yani wrap anında history YARIM kalıyordu.
    #[test]
    fn history_ring_wrap_tum_barlar() {
        const IV: u64 = 60;
        const IV_MS: i64 = (IV * 1000) as i64;
        let mut agg = agg_single_iv(IV, 5);

        // 7 kapanış için 8 bucket gerekir: her tick bir bucket ilerletir ve
        // önceki bucket O TICK'te kapanır.
        let mut closed_ts = Vec::new();
        for i in 0..8i64 {
            let t = i * IV_MS;
            for c in agg.on_trade("BTC", 100.0 + i as f64, 1.0, t, true) {
                if c.1 == IV {
                    closed_ts.push(c.2.t);
                }
            }
        }
        assert_eq!(closed_ts.len(), 7, "7 mum kapanmali");

        let h = agg.history("BTC", IV);
        assert_eq!(h.len(), 5, "history() TAM 5 mum döndürmeli");

        let ts: Vec<i64> = h.iter().map(|c| c.t).collect();
        assert!(
            ts.windows(2).all(|w| w[0] < w[1]),
            "kronolojik sıra bozuk: {ts:?}"
        );
        assert!(
            h.iter().all(|c| c.closed),
            "history() yalnız KAPANMIŞ mum içermeli"
        );

        // Son 5 KAPANMIŞ mum: bucket 2..6 (ilk 2 mum cap dışına atıldı).
        let expected: Vec<i64> = (2..7i64).map(|i| i * IV_MS).collect();
        assert_eq!(ts, expected, "son 5 kapanmış mum tam olarak beklenen olmalı");

        // ── FAZ 2: fiziksel BÖLÜNME ──
        // `as_slices()`'ın iki parça DÖNMESİ için head+len > VecDeque kapasitesi
        // gerekir. 7 kapanışta (cap=5) head=2, len=5, kapasite=8 -> 7 <= 8,
        // yani deque HENÜZ bölünmez. `as_slices().0` hatası tam da bu bölünme
        // anında görünür olduğu için literal 7 kapanış YETMEZ: bölünme
        // GÖRÜLENE kadar sürükleme yapılır ve aynı garanti her adımda
        // doğrulanır.
        let key = ("BTC".to_string(), IV);
        let mut buckets = 8i64; // sıradaki tick'in bucket'ı
        let mut splits = 0usize;
        loop {
            // B bucket'taki tick, B-1 bucket'ını kapatır.
            let t = buckets * IV_MS;
            let closed = agg.on_trade("BTC", 100.0 + buckets as f64, 1.0, t, true);
            buckets += 1;
            assert!(buckets < 64, "VecDeque fiziksel bolunmedi — on kosul saglanamadi");
            // Beklenen son-5 pencere, DÖNEN kapanan mumdan türetilir: 5 mum
            // (bu mum dâhil) geriye bakar.
            let last_closed = closed
                .iter()
                .find(|c| c.1 == IV)
                .map(|c| c.2.t)
                .expect("bu tick bir mum kapatti");
            let exp: Vec<i64> = (0..5).map(|i| last_closed - (4 - i) * IV_MS).collect();

            let (first, second) = {
                let s = agg.history.get(&key).unwrap().as_slices();
                (s.0.len(), s.1.len())
            };
            if second > 0 {
                splits += 1;
                // Bölünme anında ikinci fiziksel slice'taki mumlar KAYBOLMAMALI
                assert!(
                    first > 0 && first + second == 5,
                    "fiziksel bolunme bekleniyordu: ilk={first} ikinci={second}, toplam 5 olmali"
                );
                let h = agg.history("BTC", IV);
                assert_eq!(
                    h.len(),
                    5,
                    "bolunmede history() ikinci slice'i KAYBETTI: {:?}",
                    h.iter().map(|c| c.t).collect::<Vec<_>>()
                );
                let ts: Vec<i64> = h.iter().map(|c| c.t).collect();
                assert!(
                    ts.windows(2).all(|w| w[0] < w[1]),
                    "bolunmede sira bozuk: {ts:?}"
                );
                assert_eq!(ts, exp, "bolunmede son 5 mum yanlis: {ts:?} != {exp:?}");
            }
            if splits >= 3 {
                break; // üç ayrı bölünme adımı doğrulandı
            }
        }
    }

    /// P1-B kabul: snapshot YALNIZ mum kapanışında yeniden kurulur.
    ///
    /// `on_trade` her tick'te çağrılır (canlıda ~5 sn'de bir). Snapshot
    /// kopyalaması her tick'te yapılırsa maliyet `O(cap)` olurdu; kabul
    /// kriteri: aynı bucket içinde `history()` DEĞİŞMEZ.
    #[test]
    fn history_snapshot_sadece_kapanista_guncellenir() {
        const IV: u64 = 60;
        const IV_MS: i64 = (IV * 1000) as i64;
        let mut agg = agg_single_iv(IV, 5);

        // bucket 0 açılır
        agg.on_trade("BTC", 10.0, 1.0, 0, true);
        assert!(agg.history("BTC", IV).is_empty(), "ilk mum kapanmadan history boş");

        // bucket 1'e geç -> bucket 0 kapanır, snapshot oluşur
        agg.on_trade("BTC", 11.0, 1.0, IV_MS, true);
        let snap: Vec<(i64, f64)> = agg
            .history("BTC", IV)
            .iter()
            .map(|c| (c.t, c.c))
            .collect();
        assert_eq!(snap, vec![(0, 10.0)], "ilk kapanışta snapshot oluşmalı");

        // AYNI bucket içinde 3 tick: live mum değişir, snapshot DEĞİŞMEZ
        for (i, (px_i, px)) in [(1, 20.0), (2, 30.0), (3, 40.0)].into_iter().enumerate() {
            let ts = IV_MS + 1000 + px_i as i64 * 1000;
            let closed = agg.on_trade("BTC", px, 1.0, ts, true);
            assert!(closed.is_empty(), "ayni bucket icinde mum kapanmamali");
        }
        let live = agg.current("BTC", IV).expect("live mum var");
        assert_eq!(live.c, 40.0, "live mum son tick ile guncellenmeli");
        assert_eq!(live.h, 40.0);
        assert_eq!(live.l, 11.0);
        assert_eq!(live.n, 4, "bucket ici 4 tick birikti");

        let after: Vec<(i64, f64)> = agg.history("BTC", IV).iter().map(|c| (c.t, c.c)).collect();
        assert_eq!(
            after, snap,
            "bucket ici trade'ler snapshot'i DEGISTIRMEMELI (rebuild yalniz kapanista)"
        );

        // bucket 2'ye geç -> bucket 1 kapanır, snapshot GENİŞLER
        agg.on_trade("BTC", 50.0, 1.0, 2 * IV_MS, true);
        let grown: Vec<(i64, f64)> = agg.history("BTC", IV).iter().map(|c| (c.t, c.c)).collect();
        assert_eq!(grown.len(), 2, "kapanista snapshot guncellenmeli");
        assert_eq!(grown[1], (IV_MS, 40.0), "bucket1 son tick fiyati ile kapanmis olmali");
    }
}
