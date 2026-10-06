//! TRACK — canlı öğrenme: R kaydı, etkin ağırlıklar, Kelly kanıtı, metrikler.
//!
//! Görevler:
//! 1. `StrategyTracker::record_outcome_r`: kapanan işlemin R-çarpanını
//! GERÇEK fill verisinden pencereye (TRACK_WINDOW=100) yazar.
//! 2. `effective_weights`: n<min_n → donmuş 1.0 (cold-start); min_n..full_n
//! → dinamik Kelly formülü clamp [0.1,2.0]; n≥full_n → win<%35 → 0.
//! 3. `PerformanceMetrics`: win_rate / pf / E[R] / avg±R / maxDD_R /
//! ardışık kayıp / MFE/MAE — uydurma sayı yok (kanıt yoksa None).
//! 4. Persistence: `save_state/load_state` (atomik tmp+rename; origin="live"
//! doğrulaması — backtest çıkışı RED).
//! 5. `Health`: hata taksonomisi (transient/kritik) + ExitsOnly kipi +
//! kill-switch (karar-kritik seri).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use once_cell::sync::Lazy;

/// TID dedup ring buffer . Son ~10k transaction ID saklanır; aynı tid →
/// atla + [DEDUP] sayacı. Win/loss + R yalnız `user` kanalından gelir.
/// O(1) için HashSet (membership) + VecDeque (eviction order) kombinasyonu.
static TID_DEDUP: Lazy<Mutex<(HashSet<String>, VecDeque<String>)>> = Lazy::new(|| {
    Mutex::new((HashSet::with_capacity(10_000), VecDeque::with_capacity(10_000)))
});
const TID_DEDUP_CAP: usize = 10_000;

/// `tid_seen` — aynı transaction ID iki kez işlenirse `true` döner (atlanır).
/// O(1) membership + O(1) eviction.
pub fn tid_seen(tid: &str) -> bool {
    let mut guard = TID_DEDUP.lock().unwrap();
    let (seen, order) = &mut *guard;
    if seen.contains(tid) {
        return true;
    }
    seen.insert(tid.to_string());
    order.push_back(tid.to_string());
    if seen.len() > TID_DEDUP_CAP {
        if let Some(old) = order.pop_front() {
            seen.remove(&old);
        }
    }
    false
}

/// Trade exit kaydı (A2 JSONL şeması).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TradeExit {
    pub ts: u64,
    pub epoch: String,
    pub git: String,
    pub coin: String,
    pub side: String,
    pub strategies: Vec<String>,
    pub mfe_r: f64,
    pub mae_r: f64,
    pub dur_sn: u64,
    pub ratchet: u32,
    pub cikis: String,
    pub r_kapanis: f64,
    pub atr_bps: f64,
    pub k: f64,
}

/// Trade exit JSONL yaz (append-only, satır başına flush).
pub fn write_trade_exit(path: &str, exit: &TradeExit) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::{BufWriter, Write};
    if let Some(par) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(par);
    }
    let f = OpenOptions::new().create(true).append(true).open(path)?;
    let mut w = BufWriter::new(f);
    let line = serde_json::to_string(exit).unwrap_or_default();
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

pub const TRACK_WINDOW: usize = 100;
/// varsayılan win/loss payı (avansı: executor fill'lerinden güncellenecek)
pub const DEFAULT_PAYOFF: f64 = 1.8;
/// pencere yetersizse nötr ağırlık
pub const NEUTRAL_WEIGHT: f64 = 0.25;

// ── KEŞİF ŞERİDİ (exploration lane) ────────────────────────────────────────
// Eğitimde ağırlığı 0'a düşen (ya da hiç kanıtı olmayan) stratejiler canlıda
// küçük lotla denenir; kanıtlanırsa terfi, batarsa ceza alır.
pub const EXPLORE_FLOOR: f64 = 0.01;
pub const EXPLORE_MAX: f64 = 0.05;
pub const EXPLORE_PROMOTE_WR: f64 = 0.55;
pub const EXPLORE_PROMOTE_PF: f64 = 1.30;
pub const EXPLORE_DEMOTE_WR: f64 = 0.38;
pub const EXPLORE_DEMOTE_PF: f64 = 0.90;
/// terfi/cezası için asgari işlem sayısı (100滚 pencere içinde)
pub const EXPLORE_MIN_TRADES: usize = 30;

/// Keşif şeridindeki bir stratejinin canlı kanıt kararı.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExploreVerdict {
 /// istatistiksel olarak kanıtlandı → terfi
    Promote,
 /// ne iyi ne kötü → tabanın biraz üstü
    Hold,
 /// kanıtlandı ki kötü → yarı taban
    Demote,
 /// veri yetersiz → düz taban
    Insufficient,
}

/// Eğitim çıktısı **pay** (toplam=1) verir; `allocation_manager::conviction`
/// ise Kelly **büyüklüğü** (0.05–0.4) bekler. İkisini doğrudan karıştırmak
/// conviction'ı ~15× küçültür ve hiçbir aday eşiği geçemez. Yüklemede ölçeğe
/// çeviriyoruz: `w_kelly = pay × TRAINED_SCALE`.
pub const TRAINED_SCALE: f64 = 0.25;

// ── NÖTR COLD-START SABİTLERİ ────────────────────────────────────────
/// Nötr başlangıç ağırlığı: kanıt yokken HER strateji bu değerde.
pub const COLD_START_WEIGHT: f64 = 1.0;
/// Varsayılan `LEARN_MIN_N`: bu n altında ağırlık **hiç değişmez**.
pub const DEFAULT_LEARN_MIN_N: usize = 10;
/// Varsayılan `LEARN_FULL_N`: bu n üstünde tam kurallar + keşif kuyruğundan çıkış.
pub const DEFAULT_LEARN_FULL_N: usize = 30;
/// Ağırlık clamp aralığı (nötr=1.0 çevresinde, yani 10× dar).
pub const WEIGHT_CLAMP_LO: f64 = 0.1;
pub const WEIGHT_CLAMP_HI: f64 = 2.0;

// KALDIRILDI (bu görev): `TrainedInfo` — `Engine.trained_weights` alanı
// **hiç okunmuyordu** (ölü state): `load_live_weights` canlı dosyanın
// `weights: {name: number}` haritasını bu yapı gibi okumaya çalışıyor,
// sonuç `trades=0 / sum_r=0` ve ensemble'a hiç girmiyordu. Yerine:
// • `Engine::live_weights: HashMap<String, f64>` — kaydedilmiş canlı
// snapshot (asıl ağırlık)
// • `StrategyTracker` — canlı işlem KANITI
// İkisi de `parse_live_weights` / `merge_effective_weights` ile ayrılır.

/// Öğrenme politikası: `n` eşikleri (ENV `LEARN_MIN_N` / `LEARN_FULL_N`).
#[derive(Debug, Clone, Copy)]
pub struct LearnPolicy {
    pub min_n: usize,
    pub full_n: usize,
}

impl Default for LearnPolicy {
    fn default() -> Self {
        Self {
            min_n: env_usize("LEARN_MIN_N", DEFAULT_LEARN_MIN_N),
            full_n: env_usize("LEARN_FULL_N", DEFAULT_LEARN_FULL_N),
        }
    }
}

impl LearnPolicy {
 /// Ağırlık değiştirilebilir mi? (2-3 rastgele işlem ağırlığı öldürmesin)
    pub fn frozen(&self, n: usize) -> bool {
        n < self.min_n
    }
 /// Tam kurallar (win<%35 → 0) devreye girdi mi?
    pub fn full(&self, n: usize) -> bool {
        n >= self.full_n
    }
}

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// Keşif terfi/cezası sonrası ağırlık değişimi (log + audit için).
#[derive(Debug, Clone)]
pub struct ExploreEvent {
    pub strategy: String,
    pub verdict: ExploreVerdict,
    pub from: f64,
    pub to: f64,
    pub trades: u64,
    pub win_rate: f64,
    pub profit_factor: f64,
}

#[derive(Debug, Clone, Default)]
pub struct StratStats {
 /// (ts_ms, win) — kronolojik; cap TRACK_WINDOW
    pub outcomes: Vec<(u64, bool)>,
 /// işlem başına R-çarpanı (kâr/(başlangıç riski)); PF hesabı için.
 /// `outcomes` ile aynı uzunlukta.
    pub rs: Vec<f64>,
}

impl StratStats {
    pub fn record(&mut self, ts: u64, win: bool) {
        self.record_r(ts, if win { 1.0 } else { -1.0 });
    }

 /// Gerçek R-çarpanıyla kayıt: `r` = net PnL / giriş riski (SL mesafesi).
    pub fn record_r(&mut self, ts: u64, r: f64) {
        let r = if r.is_finite() { r.clamp(-20.0, 20.0) } else { 0.0 };
        self.outcomes.push((ts, r > 0.0));
        self.rs.push(r);
        if self.outcomes.len() > TRACK_WINDOW {
            self.outcomes.remove(0);
            self.rs.remove(0);
        }
    }

 /// kâr faktörü: toplam kazanç R / toplam kayıp |R| (pencere doluysa).
    pub fn profit_factor(&self) -> Option<f64> {
        if self.rs.is_empty() {
            return None;
        }
        let win: f64 = self.rs.iter().filter(|r| **r > 0.0).sum();
        let loss: f64 = self.rs.iter().filter(|r| **r < 0.0).sum::<f64>().abs();
        Some(if loss > 0.0 { win / loss } else if win > 0.0 { 10.0 } else { 0.0 })
    }

 /// Keşif kararı: kanıt yeterli mi, kanıt ne diyor?
    pub fn explore_verdict(&self) -> ExploreVerdict {
        if self.rs.len() < EXPLORE_MIN_TRADES {
            return ExploreVerdict::Insufficient;
        }
        let wr = self.win_rate(0).unwrap_or(0.0);
        let pf = self.profit_factor().unwrap_or(0.0);
        if wr >= EXPLORE_PROMOTE_WR && pf >= EXPLORE_PROMOTE_PF {
            ExploreVerdict::Promote
        } else if wr <= EXPLORE_DEMOTE_WR || pf < EXPLORE_DEMOTE_PF {
            ExploreVerdict::Demote
        } else {
            ExploreVerdict::Hold
        }
    }

 /// pencere win-oranı; min_trades altında None
    pub fn win_rate(&self, min_trades: usize) -> Option<f64> {
        if self.outcomes.len() < min_trades {
            return None;
        }
        let wins = self.outcomes.iter().filter(|(_, w)| *w).count();
        Some(wins as f64 / self.outcomes.len() as f64)
    }

 /// binary Kelly (f*), pencere doluyken Some
    pub fn kelly(&self, b: f64, min_trades: usize) -> Option<f64> {
        let p = self.win_rate(min_trades)?;
        Some(p - (1.0 - p) / b)
    }
}

#[derive(Debug, Default)]
pub struct StrategyTracker {
    stats: HashMap<String, StratStats>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PerformanceMetrics {
    pub trades: usize,
    pub win_rate: Option<f64>,
    pub profit_factor: Option<f64>,
    pub expectancy_r: Option<f64>,
    pub avg_win_r: Option<f64>,
    pub avg_loss_r: Option<f64>,
    pub max_drawdown_r: Option<f64>,
    pub max_consecutive_losses: Option<usize>,
    pub mfe_r: Option<f64>,
    pub mae_r: Option<f64>,
}

impl PerformanceMetrics {
 /// R dizisinden (GERÇEK fill kayıtları) metrikler.
 ///
 /// `rs` = işlem başına R-çarpanı (net PnL / giriş riski). Boş dizi =
 /// kanıt yok → tüm alanlar `None` (sıfır bile UYDURULMAZ).
    pub fn from_rs(rs: &[f64], mfe_r: Option<f64>, mae_r: Option<f64>) -> Self {
        if rs.is_empty() {
            return Self { trades: 0, mfe_r, mae_r, ..Default::default() };
        }
        let wins: Vec<f64> = rs.iter().copied().filter(|r| *r > 0.0).collect();
        let losses: Vec<f64> = rs.iter().copied().filter(|r| *r < 0.0).collect();
        let win_sum: f64 = wins.iter().sum();
        let loss_sum: f64 = losses.iter().copied().map(|r| r.abs()).sum();

 // max drawdown: kümülatif R eğrisi
        let mut cum = 0.0f64;
        let mut peak = 0.0f64;
        let mut mdd = 0.0f64;
        for r in rs {
            cum += *r;
            peak = peak.max(cum);
            mdd = mdd.max(peak - cum);
        }
 // ardışık kayıp serisi
        let mut streak = 0usize;
        let mut worst = 0usize;
        for r in rs {
            if *r <= 0.0 {
                streak += 1;
                worst = worst.max(streak);
            } else {
                streak = 0;
            }
        }
        Self {
            trades: rs.len(),
            win_rate: Some(wins.len() as f64 / rs.len() as f64),
            profit_factor: Some(if loss_sum > 0.0 { win_sum / loss_sum } else if win_sum > 0.0 { 10.0 } else { 0.0 }),
            expectancy_r: Some(rs.iter().sum::<f64>() / rs.len() as f64),
            avg_win_r: if wins.is_empty() { None } else { Some(win_sum / wins.len() as f64) },
            avg_loss_r: if losses.is_empty() { None } else { Some(-loss_sum / losses.len() as f64) },
            max_drawdown_r: Some(mdd),
            max_consecutive_losses: Some(worst),
            mfe_r,
            mae_r,
        }
    }

 /// Rapor satırı — iddia değil ölçüm.
    pub fn line(&self) -> String {
        let f = |v: Option<f64>, d: usize| v.map(|x| format!("{x:.d$}")).unwrap_or_else(|| "-".into());
        format!(
            "trades={} win_rate={} pf={} E[R]={} avg_win_R={} avg_loss_R={} maxDD_R={} ardışık_kayıp={} MFE_R={} MAE_R={}",
            self.trades,
            f(self.win_rate, 3),
            f(self.profit_factor, 2),
            f(self.expectancy_r, 3),
            f(self.avg_win_r, 2),
            f(self.avg_loss_r, 2),
            f(self.max_drawdown_r, 2),
            self.max_consecutive_losses.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
            f(self.mfe_r, 2),
            f(self.mae_r, 2),
        )
    }
}
impl StrategyTracker {
    pub fn record_outcomes(&mut self, strategy_names: Vec<String>, ts: u64, win: bool) {
        self.record_outcome_r(strategy_names, ts, if win { 1.0 } else { -1.0 });
    }

 /// Gerçek R-çarpanıyla kayıt (PF tabanlı terfi/ceza için).
    pub fn record_outcome_r(&mut self, strategy_names: Vec<String>, ts: u64, r: f64) {
        for name in strategy_names {
            self.stats.entry(name).or_default().record_r(ts, r);
        }
    }


 /// **Kanıt sayacı** (`n`) — stratejinin gerçek canlı işlem sayısı.
 ///
 /// `effective_weights`'in `pol.frozen(n)` kararıyla **aynı** girdiyi
 /// verir; canlı ağırlık snapshot'ının hangi stratejilerde korunacağı
 /// (tracker kanıtı varsa tracker kazanır) buradan belirlenir.
    pub fn evidence_n(&self, name: &str) -> usize {
        self.stats.get(name).map(|x| x.rs.len()).unwrap_or(0)
    }

 /// ETKİLİ AĞIRLIK — **nötr cold-start + canlı öğrenme** .
 ///
 /// `names` = registry'deki TÜM stratejiler (18). Ağırlık dosyası yoksa da
 /// bu listeyi kullanırız → her strateji 1.0 ile başlar.
 ///
 /// | n | ağırlık |
 /// |---|---|
 /// | `< min_n` | **1.0 sabit** (kanıt yetersiz → öldürme yok) |
 /// | `min_n ≤ n < full_n` | dinamik formül, clamp `[0.1, 2.0]` |
 /// | `n ≥ full_n` | tam kurallar: `win < %35` → **0** |
 ///
 /// Dönen `ExploreEvent` listesi yalnızca **değişen** kararları içerir
 /// (log/audit). `explorers` = veri açlığı olan stratejiler
 /// (`n < full_n`) — bkz. .
    pub fn effective_weights(
        &self,
        names: &[String],
        pol: &LearnPolicy,
    ) -> (HashMap<String, f64>, std::collections::HashSet<String>, Vec<ExploreEvent>) {
        let mut out: HashMap<String, f64> = HashMap::new();
        let mut explorers: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut events: Vec<ExploreEvent> = Vec::new();
        let mut all: Vec<String> = names.to_vec();
 // diskten yüklenmiş ama listede olmayan strateji varsa onu da izle
        for k in self.stats.keys() {
            if !all.contains(k) {
                all.push(k.clone());
            }
        }
        all.sort();

        for name in all {
            let n = self.stats.get(&name).map(|x| x.rs.len()).unwrap_or(0);
            let wr = self.stats.get(&name).and_then(|x| x.win_rate(0)).unwrap_or(0.0);
            let pf = self.stats.get(&name).and_then(|x| x.profit_factor()).unwrap_or(0.0);
            let sum_r: f64 = self.stats.get(&name).map(|x| x.rs.iter().sum()).unwrap_or(0.0);

 // nötr cold-start: kanıt yok → 1.0
            if pol.frozen(n) {
                out.insert(name.clone(), COLD_START_WEIGHT);
                if n < pol.full_n {
                    explorers.insert(name);
                }
                continue;
            }

 // f* = p − (1−p)/b, b = 1.8 varsayılan pay
            let f = self
                .stats
                .get(&name)
                .and_then(|x| x.kelly(DEFAULT_PAYOFF, pol.min_n))
                .unwrap_or(0.0);
 // nötr=1.0 çevresinde normalize: f* oranı 1.0'a göre
            let to = if pol.full(n) && wr < 0.35 {
 // tam kural: kanıt var ve berbat → SUSTUR
                0.0
            } else if f <= 0.0 {
                WEIGHT_CLAMP_LO
            } else {
 // f* ∈ (0, ~1]; 1.0'a bölerek nötre göre ölçekliyoruz.
 // f*=0.5 → w=0.5×KELLY_SCALE...
                (1.0 + f).clamp(WEIGHT_CLAMP_LO, WEIGHT_CLAMP_HI)
            };
            let to = if pol.full(n) && wr < 0.35 { 0.0 } else { to };
            if n < pol.full_n {
                explorers.insert(name.clone());
            }
 // DEĞİŞEN kararı logla (cold-start 1.0'den farklıysa)
            if (to - COLD_START_WEIGHT).abs() > 1e-9 {
                events.push(ExploreEvent {
                    strategy: name.clone(),
                    verdict: if to == 0.0 {
                        ExploreVerdict::Demote
                    } else if f > 0.0 && to > COLD_START_WEIGHT {
                        ExploreVerdict::Promote
                    } else {
                        ExploreVerdict::Hold
                    },
                    from: COLD_START_WEIGHT,
                    to,
                    trades: n as u64,
                    win_rate: wr,
                    profit_factor: pf,
                });
                let _ = sum_r;
            }
            out.insert(name, to);
        }
        (out, explorers, events)
    }

 /// **KEŞİF KUYRUĞU**: veri açlığı olan stratejiler.
 ///
 /// Yeni tanım: keşif hakkı "ağırlığı ~0 olan" değil, **veri açlığı** olan
 /// stratejiye ait. Yani `n < LEARN_FULL_N` olanlar. Ölçek/sıralama:
 /// önce en az veri görenler (`n` asc), sonra isim (deterministik).
 ///
 /// `n ≥ full_n` olan hiçbir strateji kuyruğa giremez — terfi/düşüş artık
 /// normal tracker akışında (§ kuralı).
    pub fn explore_queue(&self, names: &[String], pol: &LearnPolicy) -> Vec<String> {
        let mut q: Vec<(usize, String)> = Vec::new();
        for name in names {
            let n = self.stats.get(name).map(|x| x.rs.len()).unwrap_or(0);
            if !pol.full(n) {
                q.push((n, name.clone()));
            }
        }
        q.sort();
        q.into_iter().map(|(_, n)| n).collect()
    }

 /// I — stratejinin **ölçülebilir metrikleri** (gerçek fill R penceresi).
 /// Kanıt yok → `trades=0` ve `None`'lar (uydurma sayı yok).
    pub fn metrics(&self, name: &str) -> PerformanceMetrics {
        let rs: Vec<f64> = self.stats.get(name).map(|x| x.rs.clone()).unwrap_or_default();
        PerformanceMetrics::from_rs(&rs, None, None)
    }

 /// I — portföy geneli (tüm stratejilerin birleşik R penceresi).
    pub fn portfolio_metrics(&self) -> PerformanceMetrics {
        let mut all: Vec<(u64, f64)> = Vec::new();
        for st in self.stats.values() {
            for (i, r) in st.rs.iter().enumerate() {
                let ts = st.outcomes.get(i).map(|(t, _)| *t).unwrap_or(0);
                all.push((ts, *r));
            }
        }
        all.sort_by_key(|(t, _)| *t);
        let rs: Vec<f64> = all.into_iter().map(|(_, r)| r).collect();
        PerformanceMetrics::from_rs(&rs, None, None)
    }

 /// **PORTFÖY win-rate + k/z oranı** — tracker'daki TÜM canlı kanıttan.
 ///
 /// Kelly sizing'in sabit %45 yerine GERÇEK veri kullanabilmesi için
 /// (`RiskKelly` → `MarketInput::portfolio_*`). Kanıt yoksa `None`:
 /// sabit varsayım DÖNDÜRMEZ — çağıran (strateji) `Skip` üretir.
    pub fn portfolio_stats(&self) -> Option<(f64, f64)> {
        let wins: f64 = self.stats.values().map(|x| x.rs.iter().filter(|r| **r > 0.0).count() as f64).sum();
        let losses: f64 = self.stats.values().map(|x| x.rs.iter().filter(|r| **r < 0.0).count() as f64).sum();
        let n = wins + losses;
        if n <= 0.0 {
            return None;
        }
        let win_r: f64 = self.stats.values().map(|x| x.rs.iter().filter(|r| **r > 0.0).sum::<f64>()).sum();
        let loss_r: f64 = self
            .stats
            .values()
            .map(|x| x.rs.iter().filter(|r| **r < 0.0).sum::<f64>())
            .sum::<f64>()
            .abs();
        let wl = if loss_r > 0.0 { (win_r / wins.max(1.0)) / (loss_r / losses.max(1.0)) } else { 0.0 };
        Some((wins / n, wl))
    }

 /// Bir sonraki keşifçiyi seç: **round-robin**, sinyal üretenlerden.
 ///
 /// `cursor` döngü durumu (kalıcı değil — her döngüde sıradakine ilerler).
 /// Kuyruk + `signaled` kesişiminden **tek** strateji seçilir → aynı döngüde
 /// iki farklı keşifçiye sıra gelmez (kabul kriteri).
    pub fn next_explorer(
        &self,
        names: &[String],
        pol: &LearnPolicy,
        signaled: &std::collections::HashSet<String>,
        cursor: usize,
    ) -> Option<String> {
        let q = self.explore_queue(names, pol);
        let act: Vec<String> = q.into_iter().filter(|s| signaled.contains(s)).collect();
        if act.is_empty() {
            return None;
        }
        Some(act[cursor % act.len()].clone())
    }

 /// ensemble ağırlıkları: f*>0 → kelly pay; pencere yetersiz → nötr; kötü → 0
    pub fn ensemble_weights(&self) -> HashMap<String, f64> {
        let mut raw: HashMap<String, f64> = HashMap::new();
        for (name, s) in &self.stats {
            let w = match s.kelly(DEFAULT_PAYOFF, 15) {
                Some(f) if f > 0.0 => f,
                Some(_) if s.outcomes.len() >= 15 => 0.0, // pencere dolu ama f* korkunç
                _ => NEUTRAL_WEIGHT,
            };
            raw.insert(name.clone(), w);
        }
        raw
    }

    pub fn count(&self) -> usize {
        self.stats.len()
    }

 /// Özet: (n, win_rate, profit_factor, ΣR)
    pub fn summary(&self, name: &str) -> (usize, f64, f64, f64) {
        match self.stats.get(name) {
            None => (0, 0.0, 0.0, 0.0),
            Some(st) => (
                st.rs.len(),
                st.win_rate(0).unwrap_or(0.0),
                st.profit_factor().unwrap_or(0.0),
                st.rs.iter().sum(),
            ),
        }
    }

 // ── PERSISTENCE ( borcu #4 — artık kritik) ──────────────────────────
 //
 // Neden şart: öğrenme artık **canlı**. Restart olunca tüm n/win/PF/ΣR
 // sıfırlanırsa sistem her seferinde "18 strateji × w=1.0" cold-start'a
 // döner ve **hiçbir zaman öğrenemez** — testnet'te günlerce süren
 // toplama boşa gider. Daha kötüsü: restart sonrası ilk 10 işlem
 // nötr (w=1.0) görünür, yani öğrenilmiş ağırlıklar sessizce silinir.
 //
 // `entry_attr` (pozisyon başına, R hesabının girdisi) RAM'de kalır —
 // restart kabul edilebilir: sadece o pozisyonun kapanışında R kaydı
 // düşer, sistem kendini tutmaya devam eder.

 /// Tracker durumunu diske yaz (atomik: önce `.tmp`, sonra rename).
 /// Provenans: `origin: "live"`, `schema_version: 1` — backtest çıkışı
 /// yüklenemesin diye. **Gerçek R history** (`rs` vektörü) saklanır,
 /// restart'ta birebir yüklenir — sentetik ±1R YOK. schema_version = 2.
 /// sparse JSON bug fix: önce entry'leri topla, sonra join(",") — indeks tabanlı virgül YOK.
    pub fn save_state(&self, path: &str) -> std::io::Result<()> {
        use std::fmt::Write as _;
        let mut names: Vec<&String> = self.stats.keys().collect();
        names.sort();
        let mut entries: Vec<String> = Vec::new();
        for k in names {
            let st = &self.stats[k];
            if st.rs.is_empty() {
                continue;
            }
            let wr = st.win_rate(0).unwrap_or(0.0);
            let pf = st.profit_factor().unwrap_or(0.0);
            let sr: f64 = st.rs.iter().sum();
            let last = st.outcomes.last().map(|(t, _)| *t).unwrap_or(0);
 // R history array: [[ts, r], ...]
            let mut r_hist = String::new();
            for (idx, (ts, _win)) in st.outcomes.iter().enumerate() {
                let r = st.rs[idx];
                let _ = write!(r_hist, "[{ts},{r:.6}],");
            }
            if !r_hist.is_empty() {
                r_hist.pop(); // trailing comma
            }
            let mut entry = String::new();
            let _ = write!(
                entry,
                "    \"{k}\": {{\"n\":{},\"win\":{:.4},\"pf\":{:.4},\"sum_r\":{:.4},\"last_ts\":{},\"origin\":\"live\",\"schema_version\":2,\"r_hist\":[{r_hist}]}}",
                st.rs.len(),
                wr,
                pf,
                sr,
                last
            );
            entries.push(entry);
        }
        let strategies = entries.join(",\n");
        let s = format!("{{\n  \"origin\": \"live\",\n  \"schema_version\": 2,\n  \"strategies\": {{\n{strategies}\n  }}\n}}");
        if let Some(par) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(par);
        }
 // ATOMİK YAZIM: yarım yazılmış dosya = tüm öğrenme kaybı. tmp + rename.
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, s.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

 /// Diskten yükle. Provenans kontrolü (schema v2):
 /// `origin != "live"` → WARN + **reddet** (backtest çıkışı look-ahead biaslı)
 /// `schema_version != 2` → WARN + **reddet** (v1 sentetik R içerir, v2 gerekli)
 /// yoksa/bozuksa → false (ilk çalıştırma normal, cold-start)
 /// **Gerçek R history** (`r_hist`) ZORUNLU — sentetik ±1R fallback YOK.
    pub fn load_state(&mut self, path: &str) -> bool {
        let Ok(txt) = std::fs::read_to_string(path) else {
            return false;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
            tracing::warn!("[LEARN] {path} bozuk JSON — nötr cold-start");
            return false;
        };
 // Provenans kapısı
        let origin = v.get("origin").and_then(|x| x.as_str()).unwrap_or("");
        if origin != "live" {
            tracing::warn!("[LEARN] {path} origin='{origin}' != 'live' — REDDEDİLDİ, nötr cold-start");
            return false;
        }
        let schema = v.get("schema_version").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        if schema != 2 {
            tracing::warn!("[LEARN] {path} schema_version={schema} != 2 (v1 sentetik R içerir) — REDDEDİLDİ, nötr cold-start");
            return false;
        }
        let Some(map) = v.get("strategies").and_then(|x| x.as_object()) else {
            tracing::warn!("[LEARN] {path} şema hatalı (strategies yok) — nötr cold-start");
            return false;
        };
        let mut loaded = 0usize;
        for (name, e) in map {
 // r_hist ZORUNLU — yoksa bu strateji atlanır (sentetik R YAZMA)
            let r_hist = e.get("r_hist").and_then(|x| x.as_array());
            let st = self.stats.entry(name.clone()).or_default();
            if let Some(hist) = r_hist {
                for entry in hist {
                    if let Some(arr) = entry.as_array() {
                        if arr.len() == 2 {
                            let ts = arr[0].as_u64().unwrap_or(0);
                            let r = arr[1].as_f64().unwrap_or(0.0);
                            st.record_r(ts, r);
                        }
                    }
                }
                loaded += 1;
                continue;
            }
 // r_hist YOK → bu strateji YÜKLENMEZ (sentetik R yazmak, R hiç yazmamaktan kötüdür)
            tracing::warn!("[LEARN] {name}: r_hist eksik — strateji ATLANIYOR (sentetik R YOK)");
        }
        if loaded > 0 {
            tracing::info!(
                "[LEARN] tracker durumu yüklendi: {loaded} strateji ← {path} \
                 (gerçek R history korundu — cold-start YOK, origin=live, schema=v2)"
            );
        }
        loaded > 0
    }
}

/// **CANLI** ağırlık dökümü — `origin: "live"` işaretli.
///
/// Yükleyici (`bot.rs`) `origin != "live"` olan dosyayı **reddeder**:
/// backtest çıktısı (eski `ensemble_weights.json`) look-ahead biaslıydı
/// ve motor kaldırıldı — o dosya artık anlamsız. `origin` alanı
/// olmayan dosya da reddedilir (bilinmeyen köken = güvenilmez).
///
/// Şema:
/// ```json
/// {
/// "origin": "live",
/// "ts": 1790000000000,
/// "weights": { "Wyckoff": 0.83, "Dow Teorisi": 1.0, ... }
/// }
/// ```
pub fn live_weights_json(weights: &HashMap<String, f64>, ts: u64) -> String {
    use std::fmt::Write as _;
    let mut names: Vec<&String> = weights.keys().collect();
    names.sort();
    let mut s = String::from("{\n  \"origin\": \"live\",\n");
    let _ = write!(s, "  \"ts\": {ts},\n  \"weights\": {{\n");
    for (i, k) in names.iter().enumerate() {
        let _ = write!(s, "    \"{k}\": {:.4}", weights[*k]);
        if i + 1 < names.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push_str("  }\n}");
    s
}

/// Backtest çıktısı formatı (eski) — **artık üretilmiyor**, yalnız reddedilmek
/// üzere tanımlı ki `origin` alanı yanlışlıkla taşınmasın.
#[deprecated(note = "backtest kaldırıldı; canlı formatı `live_weights_json`")]
pub fn weights_json(weights: &HashMap<String, f64>) -> String {
    live_weights_json(weights, 0)
}

/// **[LEARN] tablosu**: strateji | w | n | win% | PF | ΣR.
pub fn learn_table(
    tracker: &StrategyTracker,
    weights: &HashMap<String, f64>,
) -> String {
    let mut names: Vec<&String> = weights.keys().collect();
    names.sort();
    let mut lines = Vec::with_capacity(names.len());
    for k in names {
        let w = weights.get(k).copied().unwrap_or(0.0);
        let (n, wr, pf, sr) = tracker.summary(k);
        lines.push(format!(
            "{:<24} w={:.2} n={:<4} win={:<5} pf={:<5} ΣR={:+.2}",
            k,
            w,
            n,
            if n > 0 { format!("{:.0}%", wr * 100.0) } else { "-".into() },
            if n > 0 { format!("{:.2}", pf) } else { "-".into() },
            sr
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|x| x.to_string()).collect()
    }
    fn pol(min_n: usize, full_n: usize) -> LearnPolicy {
        LearnPolicy { min_n, full_n }
    }
 /// n işlem kaydet: `wr` oranında kazanç (R=+1.5), kalan kayıp (R=-1.0)
    fn feed(t: &mut StrategyTracker, name: &str, n: usize, wins: usize) {
        for i in 0..n {
            let r = if i < wins { 1.5 } else { -1.0 };
            t.record_outcome_r(vec![name.to_string()], 1000 + i as u64, r);
        }
    }

 // ── KABUL KRİTERİ (a): dosya yok → hepsi 1.0 ─────────────────────────
    #[test]
    fn dosya_yoksa_hepsi_netir_bir() {
        let t = StrategyTracker::default();
        let nm = names(&["Elliott", "Wyckoff", "Kesit"]);
        let (w, ex, ev) = t.effective_weights(&nm, &pol(10, 30));
        assert_eq!(w.len(), 3);
        for k in ["Elliott", "Wyckoff", "Kesit"] {
            assert_eq!(w[k], COLD_START_WEIGHT, "{k} nötr olmalı");
        }
        assert_eq!(ex.len(), 3, "veri açlığı olan 3 strateji de kuyrukta");
        assert!(ev.is_empty(), "kanıt yokken karar değişimi olmaz");
    }

 // ── KABUL KRİTERİ (b): n=5, win=0 → ağırlık HÂLÂ 1.0 ────────────────
    #[test]
    fn bes_islem_sifir_kazanc_agirligi_olmez() {
        let mut t = StrategyTracker::default();
        feed(&mut t, "Elliott", 5, 0);
        let nm = names(&["Elliott"]);
        let (w, _, ev) = t.effective_weights(&nm, &pol(10, 30));
        assert_eq!(w["Elliott"], COLD_START_WEIGHT,
            "5 işlemde %0 kazanç ağırlığı öldürmemeli (min_n=10)");
        assert!(ev.is_empty());
    }

 // ── KABUL KRİTERİ (c): n=30, win<%35 → 0 ──────────────────────────────
    #[test]
    fn otuz_islem_dusuk_kazanc_susturur() {
        let mut t = StrategyTracker::default();
        feed(&mut t, "Elliott", 30, 6); // %20 win
        let nm = names(&["Elliott"]);
        let (w, ex, ev) = t.effective_weights(&nm, &pol(10, 30));
        assert_eq!(w["Elliott"], 0.0, "n=30 + win=%20 → sustur");
        assert!(!ex.contains("Elliott"), "full_n'a ulaşan kuyruktan çıkar");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].verdict, ExploreVerdict::Demote);
    }

    #[test]
    fn otuz_islem_iyi_kazanc_agirligi_buyur() {
        let mut t = StrategyTracker::default();
        feed(&mut t, "Elliott", 30, 24); // %80 win → f* = 0.8-0.2/1.8 = 0.689
        let nm = names(&["Elliott"]);
        let (w, ex, _) = t.effective_weights(&nm, &pol(10, 30));
        assert!(w["Elliott"] > COLD_START_WEIGHT, "iyi strateji büyümeli: {}", w["Elliott"]);
        assert!(w["Elliott"] <= WEIGHT_CLAMP_HI);
        assert!(!ex.contains("Elliott"));
    }

    #[test]
    fn on_islem_arasi_dinamik_clamp_araliginda() {
 // 12 işlem %50 win → dinamik formül devrede ama clamp [0.1, 2.0]
        let mut t = StrategyTracker::default();
        feed(&mut t, "Kesit", 12, 6);
        let nm = names(&["Kesit"]);
        let (w, ex, _) = t.effective_weights(&nm, &pol(10, 30));
        let v = w["Kesit"];
        assert!(v >= WEIGHT_CLAMP_LO && v <= WEIGHT_CLAMP_HI, "clamp dışı: {v}");
        assert!((v - COLD_START_WEIGHT).abs() > 1e-9, "n=12 artık dinamik");
        assert!(ex.contains("Kesit"), "n=12 < full_n=30 → kuyrukta");
    }

    #[test]
    fn clamp_ust_tavanini_asmaz() {
 // %100 kazanç: f* = 1.0 → 1.0+1.0 = 2.0 = tavan
        let mut t = StrategyTracker::default();
        feed(&mut t, "Kesit", 40, 40);
        let nm = names(&["Kesit"]);
        let (w, _, _) = t.effective_weights(&nm, &pol(10, 30));
        assert!((w["Kesit"] - WEIGHT_CLAMP_HI).abs() < 1e-9, "{}", w["Kesit"]);
    }

 // ── KEŞİF KUYRUĞU ─────────────────────────────────────────────
    #[test]
    fn kuyruk_veri_acligi_olana_girer_full_n_disi_da_hazir_bekler() {
        let mut t = StrategyTracker::default();
        feed(&mut t, "A", 30, 24); // full
        feed(&mut t, "B", 12, 6);  // n<full
        let nm = names(&["A", "B", "C"]); // C: n=0
        let q = t.explore_queue(&nm, &pol(10, 30));
        assert!(!q.contains(&"A".to_string()), "n≥30 kuyrukta görünmemeli");
        assert!(q.contains(&"B".to_string()));
        assert!(q.contains(&"C".to_string()));
 // en az veri gören önce
        assert_eq!(q[0], "C", "n=0 en aç → başta");
        assert_eq!(q[1], "B");
    }

    #[test]
    fn ayni_dongude_iki_kesifciye_sira_gelmez() {
        let mut t = StrategyTracker::default();
        let nm = names(&["A", "B", "C"]);
        let pol = pol(10, 30);
        let sig: std::collections::HashSet<String> = names(&["A", "B", "C"]).into_iter().collect();
        let picks: Vec<Option<String>> =
            (0..6).map(|c| t.next_explorer(&nm, &pol, &sig, c)).collect();
        for p in picks.iter() {
            assert!(p.is_some(), "sinyal üreten var → seçim yapılmalı");
        }
 // her seçim TEK strateji (Option<String>)
        assert!(picks.iter().all(|p| matches!(p, Some(_))));
 // round-robin ilerliyor
        assert_ne!(picks[0], picks[1]);
    }

    #[test]
    fn sinyal_utermeyene_zorla_islem_acilmaz() {
        let t = StrategyTracker::default();
        let nm = names(&["A", "B", "C"]);
        let pol = pol(10, 30);
        let sig: std::collections::HashSet<String> = names(&["A"]).into_iter().collect();
        assert_eq!(t.next_explorer(&nm, &pol, &sig, 0), Some("A".to_string()));
 // kuyrukta 3 strateji ama sinyal 0 → seçim yok
        let empty: std::collections::HashSet<String> = Default::default();
        assert_eq!(t.next_explorer(&nm, &pol, &empty, 0), None);
    }

    #[test]
    fn full_n_ulasilan_kuyruktan_disiyor() {
        let mut t = StrategyTracker::default();
        feed(&mut t, "A", 30, 24);
        let nm = names(&["A"]);
        let pol = pol(10, 30);
        let sig: std::collections::HashSet<String> = names(&["A"]).into_iter().collect();
        assert_eq!(t.next_explorer(&nm, &pol, &sig, 0), None,
            "n≥full_n olan bypass ALMAMALI");
    }

 // ── PERSISTENCE ( #4) ────────────────────────────────────────────
    #[test]
    fn track_state_yaz_ok_roundtrip() {
        let dir = std::env::temp_dir().join("hl_learn_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("track_state.json");
        let _ = std::fs::remove_file(&path);

        let mut t = StrategyTracker::default();
        feed(&mut t, "Elliott", 30, 24);
        feed(&mut t, "Kesit", 12, 6);
        let (w0, _, _) = t.effective_weights(&names(&["Elliott", "Kesit"]), &pol(10, 30));
        t.save_state(path.to_str().unwrap()).expect("yazma");

 // GERÇEK YENİ NESNE: sadece dosyadan okunuyor
        let mut t2 = StrategyTracker::default();
        assert!(t2.load_state(path.to_str().unwrap()), "yüklenmeli");
        let (w1, _, _) = t2.effective_weights(&names(&["Elliott", "Kesit"]), &pol(10, 30));
        assert_eq!(w0, w1, "restart sonrası ağırlıklar DEĞİŞMEMELİ (cold-start olmamalı)");
        assert_eq!(t2.summary("Elliott").0, 30);
        assert_eq!(t2.summary("Kesit").0, 12);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn track_state_olmayan_dosya_sessizce_nutre_duser() {
        let mut t = StrategyTracker::default();
        assert!(!t.load_state("/tmp/opencode/yok_boyle_bir_dosya.json"));
        let (w, _, _) = t.effective_weights(&names(&["A"]), &pol(10, 30));
        assert_eq!(w["A"], COLD_START_WEIGHT);
    }

    #[test]
    fn live_json_origin_live_uretir() {
        let mut m = HashMap::new();
        m.insert("Wyckoff".to_string(), 0.83);
        let s = live_weights_json(&m, 12345);
        assert!(s.contains("\"origin\": \"live\""), "{s}");
        assert!(s.contains("12345"));
        let v: serde_json::Value = serde_json::from_str(&s).expect("geçerli JSON");
        assert_eq!(v["origin"], "live");
        assert_eq!(v["weights"]["Wyckoff"], 0.83);
    }

 // ── PROVENANS KAPISI ────────────────────────────────────────────
    #[test]
    fn track_state_origin_degilse_reddedilir() {
        let dir = std::env::temp_dir().join("hl_learn_test_prov");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("track_state.json");
        let _ = std::fs::remove_file(&path);
 // backtestten gelmiş gibi origin="backtest" yaz
        let bad = r#"{"origin":"backtest","schema_version":2,"strategies":{"A":{"r_hist":[[1,0.5]]}}}"#;
        std::fs::write(&path, bad).unwrap();
        let mut t = StrategyTracker::default();
        assert!(!t.load_state(path.to_str().unwrap()), "origin != live → reddedilmeli");
        let (w, _, _) = t.effective_weights(&names(&["A"]), &pol(10, 30));
        assert_eq!(w["A"], COLD_START_WEIGHT, "cold-start'a düşmeli");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn track_state_schema_version_uyusmazsa_reddedilir() {
        let dir = std::env::temp_dir().join("hl_learn_test_prov2");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("track_state.json");
        let _ = std::fs::remove_file(&path);
 // origin=live ama schema_version=1 (v1 sentetik R)
        let bad = r#"{"origin":"live","schema_version":1,"strategies":{"A":{"r_hist":[[1,0.5]]}}}"#;
        std::fs::write(&path, bad).unwrap();
        let mut t = StrategyTracker::default();
        assert!(!t.load_state(path.to_str().unwrap()), "schema_version != 2 → reddedilmeli");
        let (w, _, _) = t.effective_weights(&names(&["A"]), &pol(10, 30));
        assert_eq!(w["A"], COLD_START_WEIGHT);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn track_state_origin_ve_schema_dogruysa_yuklenir() {
        let dir = std::env::temp_dir().join("hl_learn_test_prov3");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("track_state.json");
        let _ = std::fs::remove_file(&path);
 // schema v2 + r_hist with n=15 (>= min_n=10, not frozen)
        let good = r#"{"origin":"live","schema_version":2,"strategies":{"A":{"r_hist":[[1,0.5],[2,-1.0],[3,1.2],[4,0.8],[5,-0.5],[6,1.1],[7,0.9],[8,-1.2],[9,0.7],[10,1.3],[11,0.6],[12,-0.8],[13,1.0],[14,0.4],[15,-0.3]]}}}"#;
        std::fs::write(&path, good).unwrap();
        let mut t = StrategyTracker::default();
        assert!(t.load_state(path.to_str().unwrap()), "doğru provenance → yüklenmeli");
        let (w, _, _) = t.effective_weights(&names(&["A"]), &pol(10, 30));
        assert_ne!(w["A"], COLD_START_WEIGHT, "öğrenilmiş ağırlık gelmeli (n>=10, frozen değil)");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn track_state_r_hist_eksikse_strateji_atlanir_sentetik_r_yok() {
        let dir = std::env::temp_dir().join("hl_learn_test_prov4");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("track_state.json");
        let _ = std::fs::remove_file(&path);
 // r_hist yok → strateji atlanmalı
        let bad = r#"{"origin":"live","schema_version":2,"strategies":{"A":{"n":10,"win":0.5,"pf":1.0}}}"#;
        std::fs::write(&path, bad).unwrap();
        let mut t = StrategyTracker::default();
        assert!(!t.load_state(path.to_str().unwrap()), "r_hist eksik → false");
        let (w, _, _) = t.effective_weights(&names(&["A"]), &pol(10, 30));
        assert_eq!(w["A"], COLD_START_WEIGHT, "cold-start'a düşmeli");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn track_state_sparse_json_roundtrip() {
 // A boş (n=0 → rs boş), B dolu (n=30) → sparse JSON yazıp okuma
        let dir = std::env::temp_dir().join("hl_learn_test_sparse");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("track_state.json");
        let _ = std::fs::remove_file(&path);

        let mut t = StrategyTracker::default();
        feed(&mut t, "A", 0, 0);   // n=0 → rs boş (save'da atlanır)
        feed(&mut t, "B", 30, 24); // n=30 → rs dolu
        let (w0, _, _) = t.effective_weights(&names(&["A", "B"]), &pol(10, 30));
        t.save_state(path.to_str().unwrap()).expect("yazma");

 // JSON doğruluk kontrolü: trailing comma YOK, sparse entry'ler doğru
        let txt = std::fs::read_to_string(&path).unwrap();
        assert!(!txt.contains(",}"), "trailing comma YOK");
        assert!(!txt.contains(", ,"), "boş entry virgül YOK");
        assert!(txt.contains("\"B\""), "B yazılmış");
        assert!(!txt.contains("\"A\""), "A (boş) YAZILMAMIŞ");

 // Gerçek yeni nesne: sadece dosyadan okunuyor
        let mut t2 = StrategyTracker::default();
        assert!(t2.load_state(path.to_str().unwrap()), "yüklenmeli");
        let (w1, _, _) = t2.effective_weights(&names(&["A", "B"]), &pol(10, 30));
        assert_eq!(w0, w1, "restart sonrası ağırlıklar DEĞİŞMEMELİ");
        assert_eq!(t2.summary("B").0, 30);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tid_seen_aynı_tid_ikinci_kez_atlanır() {
 // ring buffer çalışıyor
        for i in 0..5 {
            assert!(!tid_seen(&format!("tid_{i}")), "ilk kez {i} → false");
        }
 // aynı tid tekrar
        assert!(tid_seen("tid_0"), "ikinci kez → true (atlandı)");
        assert!(tid_seen("tid_4"), "ikinci kez → true");
    }

    #[test]
    fn write_trade_exit_jsonl_append_only() {
        let dir = std::env::temp_dir().join("hl_trade_exit_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("trade_exits.jsonl");
        let _ = std::fs::remove_file(&path);

        let e = TradeExit {
            ts: 1_790_000_000_000,
            epoch: "2026-10-03-a".into(),
            git: "abc1234".into(),
            coin: "BTC".into(),
            side: "LONG".into(),
            strategies: vec!["Wyckoff".into()],
            mfe_r: 1.2,
            mae_r: -0.3,
            dur_sn: 3600,
            ratchet: 2,
            cikis: "tp".into(),
            r_kapanis: 1.1,
            atr_bps: 150.0,
            k: 1.5,
        };
        write_trade_exit(path.to_str().unwrap(), &e).expect("yazma 1");
        write_trade_exit(path.to_str().unwrap(), &e).expect("yazma 2");

        let txt = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = txt.lines().collect();
        assert_eq!(lines.len(), 2, "iki satır append-only");
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["coin"], "BTC");
        assert_eq!(v["epoch"], "2026-10-03-a");
        assert_eq!(v["git"], "abc1234");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pencere_100_kaydirma() {
        let mut s = StratStats::default();
        for i in 0..120 {
            s.record_r(i as u64, if i % 3 != 0 { 1.0 } else { -1.0 });
        }
        assert_eq!(s.rs.len(), TRACK_WINDOW);
        let wr = s.win_rate(100).unwrap();
        assert!((wr - 0.67).abs() < 1e-9);
    }

    #[test]
    fn oyun_disi_strateji_listeye_girilir() {
        let mut t = StrategyTracker::default();
        feed(&mut t, "Yeni", 20, 15);
        let (w, _, _) = t.effective_weights(&names(&["Elliott"]), &pol(10, 30));
        assert!(w.contains_key("Yeni"), "diskteki yeni strateji izlenmeli");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// KILL-SWITCH HATA TAKSONOMİSİ
// ═══════════════════════════════════════════════════════════════════════════
//
// ESKİ KURAL: "5 hata üst üste → flatten + dur". Bu **çok geniş** bir
// sayaçtı: WS reconnect, tekil REST timeout (retry'da başarılı), 429-after-
// backoff, stop-yerleştirme hatası — hepsi sayılıyordu. Testnet WS'i
// dakikada birkaç kez düşüyor (`502 Bad Gateway`, `Connection reset`), yani
// **normal** olan durum sistemi öldürüyordu.
//
// YENİ KURAL: sayaç yalnız **karar-kritik / durum bilinmiyor** hataları
// sayar. Bunlar: "bu emir borsada mı? bu pozisyon gerçekten kapandı mı?"
// sorularının cevabını veremediğimiz durumlar. Yanlış cevap = yanlış karar.
//
// | hata türü | sayılır mı | gerekçe |
// |---|---|---|
// | `clearinghouseState` art arda parse-fail | ✅ | pozisyon durumu BİLİNMİYOR → risk hesabı yanlış |
// | `batch_place` sonucu bilinmiyor | ✅ | emir borsada mı bilinmiyor → pozisyon ikiye katlanabilir |
// | `flatten` başarısız | ✅ | kapatma yapılmamış olabilir → stop emniyeti tek katman |
// | WS reconnect | ❌ | veri akışı kesintisi, karar etkilenmez (state REST'ten gelir) |
// | retry'da başarıya ulaşan tekil REST timeout | ❌ | `signing::post` zaten geri çekip yeniden yolluyor |
// | stop-yerleştirme hatası | ❌ | `ensure_sl` + `sync_exchange_sl` + `dedupe_sl` halleder |
// | 429-after-backoff | ❌ | backoff sonrası başarı = normal |

/// Hata sınıfı — kill-switch'in neyi sayacağı.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrClass {
 /// karar-kritik / durum bilinmiyor → sayılır
    Critical,
 /// geçici, kendi kendini toparlar → sayılmaz
    Transient,
}

/// `anyhow::Error` mesajından sınıflandır (kaynak kod değişmeden).
///
/// Neden mesajdan: hata türleri 5 ayrı modülde (`anyhow::bail!` ile) üretiliyor;
// her birine enum eklemek yerine **kalıcı bir sözleşme** (mesaj önekleri)
/// kuruyoruz. Yanlış sınıflama **güvenli** yöndedir: emin olunamayan →
/// `Critical` (eski davranış).
pub fn classify(err: &str) -> ErrClass {
    let e = err.to_ascii_lowercase();
 // ── SAYILMAYANLAR (önce bakılır — daha spesifik) ──
    const TRANSIENT: &[&str] = &[
        "reconnect",
        "websocket",
        "502",
        "503",
        "429",
        "too many requests",
        "rate limit",
        "connection reset",
        "bad gateway",
        "stop kurulamadı",
        "borsa stop kurulamadı",
        "timeout",
        "timed out",
        "eof",
    ];
    if TRANSIENT.iter().any(|t| e.contains(t)) {
        return ErrClass::Transient;
    }
 // ── SAYILANLAR (karar-kritik) ──
    const CRITICAL: &[&str] = &[
        "clearinghousestate",
        "clearinghouse_state",
        "parse",
        "bilinmiyor",
        "unknown position",
        "flatten",
        "düzleş",
        "batch place",
        "batch_place",
    ];
    if CRITICAL.iter().any(|t| e.contains(t)) {
        return ErrClass::Critical;
    }
 // EMİN OLUNAMAZ → Critical (eski davranış: say)
    ErrClass::Critical
}

/// Kill-switch / StateMaintenance durumu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
 /// normal
    #[default]
    Normal,
 /// yalnız çıkışlara izin ver (giriş emri yok) — WS/REST kararsız
    ExitsOnly,
 /// düzleştirildi, duraklatıldı
    Halted,
}

/// Taksonomiyi uygulayan sayaç.
#[derive(Debug, Clone, Default)]
pub struct Health {
    mode: Mode,
    crit_streak: usize,
 /// WS reconnect zaman damgaları (fırtına tespiti)
    ws_reconnects: Vec<u64>,
    pub critical_threshold: usize,
}

pub const WS_STORM_WINDOW_MS: u64 = 300_000;
pub const WS_STORM_COUNT: usize = 10;

impl Health {
    pub fn new(critical_threshold: usize) -> Self {
        Self { critical_threshold, ..Default::default() }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn crit_streak(&self) -> usize {
        self.crit_streak
    }

 /// WS reconnect bildir. Fırtına (5 dk'da >10) → **kill DEĞİL**, `ExitsOnly`.
    pub fn on_ws_reconnect(&mut self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.ws_reconnects.push(now);
        self.ws_reconnects.retain(|t| now.saturating_sub(*t) < WS_STORM_WINDOW_MS);
        if self.ws_reconnects.len() > WS_STORM_COUNT && self.mode == Mode::Normal {
            self.mode = Mode::ExitsOnly;
            return true;
        }
        false
    }

    pub fn ws_recent(&self) -> usize {
        self.ws_reconnects.len()
    }

 /// Döngü hatası. `true` dönerse → flatten + halt.
    pub fn on_error(&mut self, err: &str) -> bool {
        match classify(err) {
            ErrClass::Transient => {
 // sayaç **sıfırlanmaz** ama artmaz da: arka arkaya başarılı
 // döngülerde sıfırlanır (aşağıda).
                false
            }
            ErrClass::Critical => {
                self.crit_streak += 1;
                if self.crit_streak >= self.critical_threshold && self.mode != Mode::Halted {
                    self.mode = Mode::Halted;
                    true
                } else {
                    false
                }
            }
        }
    }

    pub fn on_cycle_ok(&mut self) {
        self.crit_streak = 0;
 // WS fırtınası geçtiyse normale dön (reconnect'ler durulunca)
        if self.mode == Mode::ExitsOnly && self.ws_reconnects.len() <= WS_STORM_COUNT {
            self.mode = Mode::Normal;
        }
    }

 /// Giriş emri serbest mi?
    pub fn entries_allowed(&self) -> bool {
        self.mode == Mode::Normal
    }

    pub fn reset(&mut self) {
        self.mode = Mode::Normal;
        self.crit_streak = 0;
    }
}


#[cfg(test)]
mod health_tests {
    use super::*;

    #[test]
    fn ws_hata_firtinasi_botu_oldurmaz_exits_only_gecer() {
 // KABUL KRİTERİ: WS fırtınası (5 dk'da >10 reconnect) → kill DEĞİL.
        let mut h = Health::new(5);
        for _ in 0..15 {
            h.on_ws_reconnect();
        }
        assert_eq!(h.mode(), Mode::ExitsOnly, "fırtına → ExitsOnly");
        assert!(!h.entries_allowed(), "giriş kapalı");
 // 15 reconnect KESİLME sayısına rağmen Halted DEĞİL
        assert_ne!(h.mode(), Mode::Halted);
 // hiç flatten tetiklenmedi: on_error çağrılmadı
        assert_eq!(h.crit_streak(), 0);
    }

    #[test]
    fn reconnect_sayisi_tek_basina_kill_yapmaz() {
        let mut h = Health::new(5);
        for _ in 0..9 {
            assert!(!h.on_ws_reconnect());
        }
        assert_eq!(h.mode(), Mode::Normal, "eşik altı → normal");
        assert!(h.entries_allowed());
    }

    #[test]
    fn gecici_rest_hatalari_sayilmaz() {
        let mut h = Health::new(5);
        for _ in 0..20 {
            assert!(!h.on_error("502 Bad Gateway"));
            assert!(!h.on_error("connection reset by peer"));
            assert!(!h.on_error("429 Too Many Requests"));
            assert!(!h.on_error("borsa stop kurulamadı: timeout"));
        }
        assert_eq!(h.crit_streak(), 0, "geçici hatalar sayılmamalı");
        assert_eq!(h.mode(), Mode::Normal, "bot ölmemeli");
    }

    #[test]
    fn karar_kritik_hatalar_sayilir_ve_kill_switch_uretiyor() {
        let mut h = Health::new(5);
        for _ in 0..4 {
            assert!(!h.on_error("clearinghouseState parse edilemedi (null)"));
        }
        assert_eq!(h.mode(), Mode::Normal, "4 hata < eşik 5");
        assert!(h.on_error("clearinghouseState parse edilemedi (null)"), "5. hatada kill");
        assert_eq!(h.mode(), Mode::Halted);
    }

    #[test]
    fn basarili_dongu_sayaci_sifirlar() {
        let mut h = Health::new(5);
        for _ in 0..4 {
            h.on_error("clearinghouseState parse edilemedi");
        }
        h.on_cycle_ok();
        assert_eq!(h.crit_streak(), 0);
 // 4 hata daha yetmez (eşiğe ulaşmadı)
        for _ in 0..4 {
            assert!(!h.on_error("clearinghouseState parse edilemedi"));
        }
        assert_eq!(h.mode(), Mode::Normal);
    }

    #[test]
    fn emin_olunamayan_hata_kritik_sayilir() {
 // "bilinmeyen durum" = karar-kritik. Güvenli yön varsayılanı.
        assert_eq!(classify("harika bir hata"), ErrClass::Critical);
        assert_eq!(classify("bilinmeyen bir sey oldu"), ErrClass::Critical);
    }

    #[test]
    fn emin_olunamayan_hata_daha_spesifik_geceni_yener() {
 // 502 hem "transient" listesinde hem "parse" değil → Transient kazanır
        assert_eq!(classify("502 Bad Gateway"), ErrClass::Transient);
 // timeout transient olmalı — "stop kurulamadı" da öyle
        assert_eq!(classify("stop kurulamadı"), ErrClass::Transient);
    }

    #[test]
    fn exits_only_kipinde_giris_yok_cikis_var() {
        let mut h = Health::new(5);
        for _ in 0..12 {
            h.on_ws_reconnect();
        }
        assert!(!h.entries_allowed());
        h.on_cycle_ok();
        h.reset();
        assert!(h.entries_allowed(), "reset → normal");
    }
}
