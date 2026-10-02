//! CANLI İSTATİSTİK İZLEYİCİ — dinamik Kelly geri beslemesi.
//!
//! AMAÇ: "son 1 yılda %60 idi ama son 20 işlemde %30'a düştü" durumunda
//! sistem kendi kendini düzeltsin:
//!   - her kapanan pozisyon → katkı veren stratejilere (win/loss) işlenir
//!   - ROLLING WINDOW: son 100 işlem (eski kayıt: pencere dışına düşer)
//!   - f* = p − (1−p)/b pencereden; b = 1.8 (win/loss payı, sabit başlangıç)
//!   - win-rate <%35 → ensemble ağırlığı 0; f*>0 → pay
//!
//! Attribution: emir açılışında katkı veren stratejiler kaydedilir
//! (allocation_manager::Vote.voters).

use std::collections::HashMap;

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

/// Backtest eğitiminden gelen strateji künyesi.
/// **KEŞİF SINIFI YALNIZCA `trades == 0` olanlar içindir** — düşük ağırlık alan
/// ama işlem yapan strateji (ör. Wyckoff 0.02) KURULMUŞ sayılır: normal Kelly
/// dilimlemesiyle çalışır, keşif indirimi/tavanı UYGULANMAZ.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrainedInfo {
    /// normalize PAY (toplam=1) — `combine()` oranları için doğru ölçek
    pub weight: f64,
    /// HAM Kelly büyüklüğü (trainer `score`) — conviction ölçeği budur.
    /// `weight` pay olduğu için conviction'a doğrudan girilemez; 0 ise
    /// geriye dönük uyum için `weight` kullanılır.
    pub kelly: f64,
    /// backtestteki toplam işlem sayısı (IS)
    pub trades: u64,
}

impl TrainedInfo {
    pub fn is_explorer(&self) -> bool {
        self.trades == 0
    }
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

    /// ETKİLİ AĞIRLIK = katmanlı birleştirme (eğitim önceliği + canlı kanıt):
    ///  1) eğitim ağırlığı >= EXPLORE_MAX → KURULMUŞ: `w = eğitim × kelly_çarpanı`
    ///     (canlı f*<=0 ise 0'a iner → kazanan strateji anında susar)
    ///  2) eğitim ağırlığı < EXPLORE_MAX → KEŞİF: taban/terfi/ceza (max EXPLORE_MAX)
    ///
    /// Dönen `ExploreEvent` listesi yalnızca değişen kararları içerir (log/audit).
    pub fn effective_weights(
        &self,
        trained: &HashMap<String, TrainedInfo>,
        explore_enabled: bool,
    ) -> (HashMap<String, f64>, std::collections::HashSet<String>, Vec<ExploreEvent>) {
        let mut out: HashMap<String, f64> = HashMap::new();
        let mut explorers: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut events: Vec<ExploreEvent> = Vec::new();
        let mut names: Vec<String> = trained.keys().cloned().collect();
        for k in self.stats.keys() {
            if !trained.contains_key(k) {
                names.push(k.clone());
            }
        }
        names.sort();

        for name in names {
            let info = trained.get(&name).copied().unwrap_or_default();
            let t = info.weight;
            let st = self.stats.get(&name);
            // KURULMUŞ = backtestte işlem YAPMIŞ (ağırlık ne olursa olsun)
            if !info.is_explorer() {
                // kurulmuş strateji: canlı Kelly çarpanıyla ölçeklenir
                let mult = match st.and_then(|s| s.kelly(DEFAULT_PAYOFF, 15)) {
                    Some(f) if f <= 0.0 => 0.0,
                    Some(f) => (f / NEUTRAL_WEIGHT).clamp(0.0, 1.5),
                    None => 1.0, // kanıt yok → eğitim öncelikli, ceza uygulanmaz
                };
                // pay → Kelly büyüklüğü ölçeği, sonra canlı çarpan
                // ÖLÇEK: ham Kelly büyüklüğü × parça-Kelly. `weight` (pay)
            // kullanılırsa conviction ~0.02'ye düşer ve min_conviction
            // (0.05) hiç geçilemez → bot hiç pozisyon açmaz.
            let base = if info.kelly > 0.0 { info.kelly } else { t };
            out.insert(name.clone(), base * TRAINED_SCALE * mult);
                continue;
            }
            if !explore_enabled {
                explorers.insert(name.clone());
                out.insert(name, 0.0);
                continue;
            }
            let n = st.map(|x| x.rs.len()).unwrap_or(0);
            let wr = st.and_then(|x| x.win_rate(0)).unwrap_or(0.0);
            let pf = st.and_then(|x| x.profit_factor()).unwrap_or(0.0);
            let verdict = st.map(|x| x.explore_verdict()).unwrap_or(ExploreVerdict::Insufficient);
            let to = match verdict {
                ExploreVerdict::Promote => EXPLORE_MAX,
                ExploreVerdict::Hold => EXPLORE_FLOOR * 1.5,
                ExploreVerdict::Demote => EXPLORE_FLOOR * 0.5,
                ExploreVerdict::Insufficient => EXPLORE_FLOOR,
            };
            if verdict == ExploreVerdict::Promote || verdict == ExploreVerdict::Demote {
                events.push(ExploreEvent {
                    strategy: name.clone(),
                    verdict,
                    from: t,
                    to,
                    trades: n as u64,
                    win_rate: wr,
                    profit_factor: pf,
                });
            }
            explorers.insert(name.clone());
            out.insert(name, to);
        }
        (out, explorers, events)
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
}

/// JSON ağırlık dosyası formatı (ensemble::load_weights_json ile uyum şartında)
pub fn weights_json(weights: &HashMap<String, f64>) -> String {
    weights_json_ex(weights, &HashMap::new())
}

/// Ağırlık dökümü + keşif rozetleri.
/// `learned`: backtest önseli (eğitimden gelen), `weight`: canlı etkin ağırlık.
/// Keşif stratejileri `explore:true` ile işaretlenir → yeniden yüklemede
/// yalnızca EXPLORE_MAX'a kadar yükseltilebilir (üst üste terfi kazandırmaz).
pub fn weights_json_ex(
    weights: &HashMap<String, f64>,
    learned: &HashMap<String, TrainedInfo>,
) -> String {
    use std::fmt::Write as _;
    let mut names: Vec<&String> = weights.keys().collect();
    names.sort();
    let mut s = String::from("{\n");
    for (i, k) in names.iter().enumerate() {
        let v = weights[*k];
        let li = learned.get(*k).copied().unwrap_or_default();
        let l = li.weight;
        let is_explore = li.is_explorer();
        let _ = write!(
            s,
            "  \"{k}\":{{\"weight\":{:.4},\"learned\":{:.4},\"learned_trades\":{},\"explore\":{},\"explore_cap\":{:.3},\"win_rate\":0.0,\"profit_factor\":0.0,\"sharpe\":0.0,\"overfit\":false}}",
            v,
            l,
            li.trades,
            is_explore,
            EXPLORE_MAX
        );
        if i + 1 < names.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push('}');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pencere_100_kaydirma() {
        let mut s = StratStats::default();
        for i in 0..120 {
            s.record(i as u64, i % 3 != 0); // %66.7
        }
        assert_eq!(s.outcomes.len(), TRACK_WINDOW);
        // pencere: son 100 → hepsi %66 (i 20..120)
        let wr = s.win_rate(100).unwrap();
        assert!((wr - 0.67).abs() < 1e-9);
    }

    #[test]
    fn agirliklar_dusen_strateji() {
        let mut t = StrategyTracker::default();
        // iyi strateji: %70 kayıplı değil
        for i in 0..40 {
            t.record_outcomes(vec!["iyi".into()], i as u64 * 1000, i % 10 != 3);
        }
        // kötü strateji: %20
        for i in 0..40 {
            t.record_outcomes(vec!["kotu".into()], i as u64 * 1000, i % 10 == 2);
        }
        let w = t.ensemble_weights();
        assert!(w["kotu"] < 0.01, "kötü ağırlık 0:{}:", w["kotu"]);
        assert!(w["iyi"] > 0.3, "iyi ağırlık yüksek: {}", w["iyi"]);
        t.record_outcomes(vec!["yeni".into()], 999, true);
        assert_eq!(t.count(), 3);
        // yetersiz pencere → nötr 0.25
        assert_eq!(w.get("yeni").copied(), None);
    }
    // ── KEŞİF ŞERİDİ TESTLERİ ──────────────────────────────────────────────
    fn trained_zero_and_high() -> HashMap<String, TrainedInfo> {
        HashMap::from([
            // kurulmuş: 809 işlem
            ("Elliott".to_string(), TrainedInfo { weight: 0.667, kelly: 3.6, trades: 809 }),
            // keşif: 0 işlem (veri beslenemedi)
            ("Kesit".to_string(), TrainedInfo { weight: 0.0, kelly: 0.0, trades: 0 }),
        ])
    }

    #[test]
    fn dusuk_agirligi_li_islem_yapan_keşif_sayilmaz() {
        // Wyckoff: ağırlık 0.020 (düşük) ama 5897 işlem → KURULMUŞ olmalı
        let mut t = StrategyTracker::default();
        for i in 0..40 {
            t.record_outcome_r(vec!["Wyckoff".into()], 1000 + i as u64, if i % 4 == 0 { -1.0 } else { 1.0 });
        }
        let mut trained = HashMap::new();
        trained.insert("Wyckoff".to_string(), TrainedInfo { weight: 0.020, kelly: 0.107, trades: 5897 });
        let (w, explorers, _) = t.effective_weights(&trained, true);
        assert!(!explorers.contains("Wyckoff"), "işlem yapan strateji keşif OLAMAZ");
        assert!(w["Wyckoff"] > 0.0, "düşük ağırlıklı kurulmuş strateji sıfırlanmamalı");
    }

    #[test]
    fn islem_yapan_sifir_agirligi_li_kurulmus_kalir() {
        // trades>0 ama ağırlık 0 (overfit cezası) → keşif DEĞİL, sessiz kalır
        let mut t = StrategyTracker::default();
        let mut trained = HashMap::new();
        trained.insert("Kesit".to_string(), TrainedInfo { weight: 0.0, kelly: 0.0, trades: 4200 });
        let (w, explorers, _) = t.effective_weights(&trained, true);
        assert!(!explorers.contains("Kesit"));
        assert_eq!(w["Kesit"], 0.0);
    }

    #[test]
    fn egitim_payi_kelly_buyuklugune_cevrilir() {
        // Elliott: pay 0.667, HAM Kelly büyüklüğü 3.6 → 0.25 kesirli Kelly
        // sonucu 0.9. Pay (0.167) kullanılsaydı conviction 0.05 eşiğinin
        // altında kalır ve bot hiç pozisyon açmazdı.
        let mut t = StrategyTracker::default();
        let (w, _x, _) = t.effective_weights(&trained_zero_and_high(), true);
        assert!((w["Elliott"] - 3.6 * TRAINED_SCALE).abs() < 1e-9,
            "ölçek çevrimi uygulanmadı: {}", w["Elliott"]);
        assert!(w["Elliott"] > 0.30 && w["Elliott"] < 1.20,
            "Kelly büyüklüğü aralığı dışında: {}", w["Elliott"]);
    }

    #[test]
    fn sifir_agirligi_taban_agirligi_alir() {
        let mut t = StrategyTracker::default();
        let (w, _x, ev) = t.effective_weights(&trained_zero_and_high(), true);
        assert_eq!(w["Kesit"], EXPLORE_FLOOR, "keşif tabanı uygulanmalı");
        assert!(
            (w["Elliott"] - 3.6 * TRAINED_SCALE).abs() < 1e-9,
            "kanıtsız kurulmuş strateji EĞİTİM değerinde (ölçekli) kalmalı, got {}",
            w["Elliott"]
        );
        assert!(ev.is_empty(), "veri yokken terfi/ceza olmaz");
    }

    #[test]
    fn kesif_kapaliyken_sifir_kalir() {
        let mut t = StrategyTracker::default();
        let (w, _x, _) = t.effective_weights(&trained_zero_and_high(), false);
        assert_eq!(w["Kesit"], 0.0, "keşif kapalıyken ağırlık 0 olmalı");
    }

    #[test]
    fn kanitlanan_keşif_terfi_edilir() {
        let mut t = StrategyTracker::default();
        // 40 işlem: 26 kazanç R=+1.5, 14 kayıp R=-1 → wr=0.65, pf=2.79
        for i in 0..40 {
            let r = if i % 3 != 0 { 1.5 } else { -1.0 };
            t.record_outcome_r(vec!["Kesit".into()], 1000 + i as u64, r);
        }
        let (w, _x, ev) = t.effective_weights(&trained_zero_and_high(), true);
        assert_eq!(w["Kesit"], EXPLORE_MAX, "kanıtlanan keşif terfi etmeli");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].verdict, ExploreVerdict::Promote);
        assert!(ev[0].profit_factor > 1.3);
    }

    #[test]
    fn kanitlanan_kotu_kesif_cezalandirilir() {
        let mut t = StrategyTracker::default();
        for i in 0..40 {
            let r = if i % 5 == 0 { 0.8 } else { -1.0 };
            t.record_outcome_r(vec!["Kesit".into()], 1000 + i as u64, r);
        }
        let (w, _x, ev) = t.effective_weights(&trained_zero_and_high(), true);
        assert!(w["Kesit"] < EXPLORE_FLOOR, "cezalanan keşif tabanın altına inmeli");
        assert_eq!(ev[0].verdict, ExploreVerdict::Demote);
    }

    #[test]
    fn kotu_live_kelly_kurulmus_stratejiyi_susturur() {
        let mut t = StrategyTracker::default();
        // 100 işlem: wr=0.30 → f* = 0.3 - 0.7/1.8 = -0.089 < 0 → ağırlık 0
        for i in 0..100 {
            let r = if i % 10 < 3 { 1.0 } else { -1.0 };
            t.record_outcome_r(vec!["Elliott".into()], 1000 + i as u64, r);
        }
        let (w, _x, _) = t.effective_weights(&trained_zero_and_high(), true);
        assert_eq!(w["Elliott"], 0.0, "f*<0 olan kurulmuş strateji susmalı");
    }

    #[test]
    fn iyi_live_kelly_agirligi_buyutur() {
        let mut t = StrategyTracker::default();
        // wr=0.70 → f* = 0.7 - 0.3/1.8 = 0.533 → çarpan 0.533/0.25 = 2.13 → 1.5 (tavan)
        for i in 0..100 {
            let r = if i % 10 < 7 { 1.0 } else { -1.0 };
            t.record_outcome_r(vec!["Elliott".into()], 1000 + i as u64, r);
        }
        let (w, _x, _) = t.effective_weights(&trained_zero_and_high(), true);
        assert!((w["Elliott"] - 3.6 * TRAINED_SCALE * 1.5).abs() < 1e-9, "canlı kazanç ağırlığı artırmalı");
    }

    #[test]
    fn oyun_disi_strateji_keşfe_girmez() {
        let mut t = StrategyTracker::default();
        for i in 0..40 {
            t.record_outcome_r(vec!["Yeni".into()], 1000 + i as u64, 2.0);
        }
        let (w, _x, _) = t.effective_weights(&trained_zero_and_high(), true);
        assert_eq!(w["Yeni"], EXPLORE_MAX);
    }

    #[test]
    fn r_carpani_istatistikleri_bozmaz() {
        let mut st = StratStats::default();
        for _ in 0..150 {
            st.record_r(1, 3.0); // büyük kâr
        }
        assert_eq!(st.rs.len(), TRACK_WINDOW, "pencere korunmalı");
        assert_eq!(st.profit_factor(), Some(10.0), "zararsız pencere → tavanlı PF");
        assert_eq!(st.explore_verdict(), ExploreVerdict::Promote);
    }

}

impl StrategyTracker {
    /// pozisyon özeti: (stratadı, işlem sayısı, win-oranı)
    pub fn perf_summary(&self) -> Vec<(String, u64, f64)> {
        self.stats
            .iter()
            .filter_map(|(k, s)| s.win_rate(1).map(|wr| (k.clone(), s.outcomes.len() as u64, wr)))
            .collect()
    }

}
