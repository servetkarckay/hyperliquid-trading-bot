//! ALLOCATION MANAGER — sermaye tahsisi ve TEK maliyet modeli.
//!
//! Görevler:
//! 1. `conviction`: ağırlık-payı × genişlik × güç / volatilite → aday
//! conviction'ı; `allocate`: min_conviction + korelasyon (>%70 LOOSE
//! üstü çakışan düşürülür) + keşif indirimi → `Candidate`.
//! 2. `sl_tp`: SL = mid ± sl_atr_mult×ATR; TP `tp_from_entry` TEK
//! formül: max((tp/sl)×risk, 2×risk), gerçek dolumdan).
//! 3. `CostModel` EV kapısı ile yürütme bandı TEK modelden —
//! `expected_slippage(spread) = base + 0.5×spread`,
//! `execution_cost = fee + slip + spread`, taban = max(EV_MIN_DIST, 3×rt).
//! 4. MIN_NOTIONAL_USD=$10 altı tahsis → emir üretme.

use std::collections::HashMap;

use strategy::ensemble::Dir;

/// rahat mod korelasyon eşiği
pub const LOOSE_CORR: f64 = 0.70;
/// Hyperliquid minimum emri ~$10; altındaki tahsis borsada reddedilir.
pub const MIN_NOTIONAL_USD: f64 = 10.0;

/// Seçilmiş aday (motorun emir üretmeden önceki kararı)
#[derive(Debug, Clone)]
pub struct Candidate {
    pub coin: String,
    pub dir: Dir,
 /// ensemble net skoru [-1,1]
    pub score: f64,
 /// bu yönde oy veren strateji sayısı
    pub votes: usize,
 /// oy veren stratejilerin ağırlık ortalaması (dinamik kelly beslenir)
    pub kelly_weight: f64,
 /// ATR$/mid — risk marjı (yüksek = riskli)
    pub atr_pct: f64,
 /// conviction = (yönlü ağırlık payı) × (genişlik) × (güç) / (1 + volatilite)
 ///
 /// Ölçü **oy sayısı değil ağırlık payıdır**: ağırlık tabanlı bir ensemble'da
 /// 3 stratejinin 1 oyu, 12 stratejinin 12 oyundan daha ağır olabilir.
 /// Eski formül (`votes/18`) mevcut ağırlık ölçeğiyle ~0.03 üretiyor ve
 /// `min_conviction`'ın altında kalıyordu → bot hiç pozisyon açmıyordu.
    pub conviction: f64,
 /// tahsis edilmiş USD (tahsis fazından sonra dolu)
    pub allocated_usd: f64,
 /// katkı veren stratejiler (attribution)
    pub voters: Vec<String>,
 /// ATR tabanlı SL (karar anı `mid`'e göre; long: −, short: +).
    pub sl_px: f64,
 /// **PLANLANAN** TP hedefi — tahsis anındaki karar `mid`'ine göre
 /// (`tp_from_entry(mid, risk)`). Otoriter analytical hedef DEĞİL: gerçek
 /// dolum sonrası `EntryAttr::tp_px` üzerinden **yeniden türetilir**
 /// (`Engine::apply_real_fill` → `EntryAttr::set_analytical_tp`).
 ///
 /// ⚠️ Bugün TP bir kapanış kaynağı değil (analytical target); borsada TP
 /// limit emri asılmaz, `exit_reason=TakeProfit` hiçbir yolda atanmaz.
    pub tp_px: f64,
 /// oy veren keşif stratejilerinin payı (0..1); >0 ise keşif riski sınırlıdır
    pub explorer_share: f64,
 /// keşif tavanı uygulandı mı
    pub capped: bool,
}

#[derive(Debug, Clone)]
pub struct AllocationConfig {
    pub top_n: usize,
 /// çeşitlendirme zorunluluğu eşiği
    pub max_correlation: f64,
    pub sl_atr_mult: f64,
    pub tp_atr_mult: f64,
    pub min_conviction: f64,
    pub votes_norm: usize, // toplam strateji sayısı (normalizasyon: 18)
 /// pozisyon başına tavan (USD) — risk kapısıyla AYNI değer olmalı,
 /// yoksa tahsis riski reddediyor ve sinyal boşa gidiyor
    pub max_position_usd: f64,
 /// saf keşif adayının tek pozisyon tavanı (USD)
    pub explore_max_usd: f64,
 /// toplam keşif sermayesi tavanı (bütçenin oranı)
    pub explore_cap_frac: f64,
 /// keşif oyununun conviction indirimi (karıştırıcılık cezası)
    pub explore_discount: f64,
 /// F — TEK MALİYET MODELİ (EV + yürütme bandı ortak)
    pub cost: CostModel,
}

impl AllocationConfig {
 /// **TEK TP FORMÜLÜ** — her yer bu çağırır .
 ///
 /// ```text
 /// tp_dist = (tp_atr_mult / sl_atr_mult) × risk_mesafesi
 /// tp_dist = max(tp_dist, 2 × risk_mesafesi) // yan kapı
 /// TP(LONG) = entry_eff + tp_dist
 /// TP(SHORT)= entry_eff - tp_dist
 /// ```
 ///
 /// `entry_eff` = **gerçek dolum** fiyatı (REST `entryPx`), karar anı
 /// defter fiyatı DEĞİL. Kayma TP'yi de kaydırır → TP daima dolumun
 /// kâr tarafında kalır.
 /// `risk_mesafesi` = |entry_eff − gerçek SL| = `EntryAttr::risk_px`
 /// (stop mesafesi entry'ye göre taşınır, bu yüzden kayma riski değiştirmez).
 /// Yan kapı: `tp_dist ≥ 2 × risk_mesafesi`. R:R oranı 1'den küçükse bile
 /// TP asla girişin arkasına (zarar tarafına) düşmez.
 ///
 /// ✅ `TakeProfit` **GERÇEK kapanış kaynağıdır** giriş dolumunda
 /// `pending_tp` mesafesiyle reduce-only TP trigger borsaya asılır
 /// (`Executor::ensure_tp`), TP fill oid eşleşmesiyle
 /// `exit_reason=TakeProfit` atanır. Bu formül hem plan hem gerçek emrin
 /// TEK kaynağıdır.
    pub fn tp_from_entry(&self, dir: Dir, entry_eff: f64, risk_mesafesi: f64) -> f64 {
        let risk = risk_mesafesi.abs();
        if !(entry_eff > 0.0) || !(risk > 0.0) {
            return entry_eff;
        }
        let min_tp_dist = risk * 2.0;
        let tp_dist = ((self.tp_atr_mult / self.sl_atr_mult) * risk).max(min_tp_dist);
        match dir {
            Dir::Long => entry_eff + tp_dist,
            Dir::Short => entry_eff - tp_dist,
            Dir::Neutral => entry_eff,
        }
    }
}

impl Default for AllocationConfig {
    fn default() -> Self {
        Self {
            top_n: 5,
            max_correlation: 0.70,
            sl_atr_mult: 1.5,
            tp_atr_mult: 3.0,
 // Ölçek (kelly büyüklüğü) kalibrasyonu:
 // Elliott öncülü 3 oy → 0.039; zayıf karışık oy ~0.001, keşif oyu
 // ayrı baypas. Ağırlık-payı ölçeği: %12+ sermaye yönlü (güç ve
 // volatilite indirimi sonrası). Tek zayıf oy ~0.005.
 // Ağırlık-payı ölçeğiyle: %12+ sermaye yönlü (güç ve volatilite
 // indirimi sonrası). Tek zayıf oy ~0.005, keşif oyu ayrı baypas.
            min_conviction: 0.12,
            votes_norm: 18,
            max_position_usd: 50.0,
            explore_max_usd: 150.0,
            explore_cap_frac: 0.05,
            explore_discount: 0.7,
            cost: CostModel::from_env(),
        }
    }
}

/// Tek aday girdisi (motorun bu döngüdeki değerlendirmesi)
pub struct Vote {
    pub coin: String,
    pub dir: Dir,
    pub score: f64,
 /// bu yönde oy veren stratejilerin isimleri (attribution)
    pub voters: Vec<String>,
 /// strateji ağırlık haritası (dinamik kelly)
    pub weights: HashMap<String, f64>,
 /// AÇIK keşif kümesi (eğitimsiz → taban ağırlıklı). Eşikle değil, isimle
 /// sınıflandırılır: eğitimle küçük ağırlık alan strateji KURULMUŞ sayılır.
    pub explore: std::collections::HashSet<String>,
 /// ATR$/mid (risk marjı)
    pub atr_pct: f64,
 /// son fiyat (SL/TP hesabı)
    pub mid: f64,
}

pub struct AllocationManager {
    pub cfg: AllocationConfig,
}

impl AllocationManager {
    pub fn new(cfg: AllocationConfig) -> Self {
        Self { cfg }
    }

 /// İNANÇ SKORU: (oy yoğunluğu × kelly ağırlığı × ensemble gücü) / risk marjı
    pub fn conviction(
        &self,
        w_sum: f64,
        w_total: f64,
        n_voters: usize,
        atr_pct: f64,
        score_abs: f64,
    ) -> f64 {
// **NORMALİZASYON NOTU (bilinçli ayrım)**: ensemble skoru YALNIZ oy
                // verenlerin ağırlıklarıyla normalize eder ("sinyal ne kadar
                // güçlü?"); tahsisin `share`'i TÜM strateji ağırlıklarını
                // denominator yapar ("sermayenin yüzde kaçı bu yönde?").
                // İki farklı SORU — kasıtlı katman ayrımı.
 // ensemble sermayesinin ne kadarı bu yöne oy veriyor (0..1)
        let share = (w_sum / w_total.max(1e-9)).clamp(0.0, 1.0);
 // kaç strateji katılıyor: tek güçlü oy = 1.0, 6+ oyuncu = 1.5 (tavan)
        let breadth = (1.0 + 0.1 * (n_voters as f64 - 1.0)).clamp(0.5, 1.5);
        let strength = 0.6 + 0.4 * score_abs.clamp(0.0, 1.0);
        let risk = 1.0 + atr_pct * 20.0; // %5 ATR ≈ 2× marj → yarıma düşer
        (share * breadth * strength / risk).clamp(0.0, 1.0)
    }

 /// SL fiyatı: long → mid−slA·ATR / short → mid+slA·ATR.
 ///
 /// TP **ayrı formülle hesaplanmaz**; `tp_from_entry` çağrılır, yani
 /// tahsis anındaki `mid` üzerinden **planlanan** (henüz dolum olmamış)
 /// hedef verilir. Gerçek dolum sonrası TP yeniden türetilir
 /// (`EntryAttr::set_analytical_tp`). `sl_atr_mult·ATR` risk mesafesi
 /// verildiği için iki yol **matematiksel olarak aynı** sonucu üretir —
 /// fark yalnız `entry` kaymasında görünür, ki o zaten ikinci yolda
 /// düzeltilir.
    pub fn sl_tp(&self, dir: Dir, mid: f64, atr_usd: f64) -> (f64, f64) {
        let risk = self.cfg.sl_atr_mult * atr_usd;
        let sl = match dir {
            Dir::Long => mid - risk,
            Dir::Short => mid + risk,
            Dir::Neutral => mid,
        };
        (sl, self.tp_from_entry(dir, mid, risk))
    }

 /// Gerçek TP hedefi: `AllocationConfig::tp_from_entry` (tek formül).
    pub fn tp_from_entry(&self, dir: Dir, entry_eff: f64, risk_mesafesi: f64) -> f64 {
        self.cfg.tp_from_entry(dir, entry_eff, risk_mesafesi)
    }

 /// TAM KANAT: sırala → korelasyonla ele → tahsis.
 /// `corr_at`: (i,j) coin çifti getiri korelasyonu (math::portfolio'dan)
    pub fn allocate(
        &self,
        votes: Vec<Vote>,
        corr_at: &dyn Fn(&str, &str) -> f64,
        budget_usd: f64,
    ) -> Vec<Candidate> {
 // 1) adaylar: yönlü + min conviction
        let n_votes_in = votes.len();
        let mut cands: Vec<Candidate> = votes
            .into_iter()
            .filter(|v| v.dir != Dir::Neutral && v.mid > 0.0)
            .map(|v| {
 // oy ağırlıkları: açık keşif kümesi vs kurulmuş
                // BAĞIMSIZLIK DÜZELTMESİ: genişlik ödülü HAM oy sayısına değil,
                // oyların BİLGİ KÜMESİ sayısına göre — aynı kümeden 5 strateji =
                // aynı faktörün 5 formülasyonu = 1 bağımsız kanıt.
                let n_voters_raw = v.voters.len();
                let distinct_clusters = {
                    let mut cs = std::collections::HashSet::new();
                    for n in &v.voters {
                        cs.insert(strategy::info_cluster(n));
                    }
                    cs.len()
                };
                let mut kw_sum = 0.0;
                let mut n_explore = 0usize;
                for n in &v.voters {
                    let w = v.weights.get(n).copied().unwrap_or(0.0);
                    if v.explore.contains(n) {
                        n_explore += 1;
                    }
                    kw_sum += w;
                }
                let kw = if n_voters_raw == 0 { 0.0 } else { kw_sum / n_voters_raw as f64 };
                let explorer_share = n_explore as f64 / n_voters_raw.max(1) as f64;
                let w_total: f64 = v.weights.values().sum::<f64>();
                let mut conv =
                    self.conviction(kw_sum, w_total, distinct_clusters, v.atr_pct, v.score.abs());
 // karıştırıcılık cezası: keşif oyu inandırıcılığı düşürür
                conv *= 1.0 - self.cfg.explore_discount * explorer_share;
                let atr_usd = v.atr_pct * v.mid;
                let (sl, tp) = self.sl_tp(v.dir, v.mid, atr_usd);
                Candidate {
                    coin: v.coin,
                    dir: v.dir,
                    score: v.score,
                    votes: v.voters.len(),
                    kelly_weight: kw,
                    atr_pct: v.atr_pct,
                    conviction: conv,
                    allocated_usd: 0.0,
                    voters: v.voters.clone(),
                    sl_px: sl,
                    tp_px: tp,
                    explorer_share,
                    capped: false,
                }
            })
 // Keşif adayı (yalnız keşif oyu): conviction taban ağırlığı yüzünden
 // matematiksel olarak min_conviction'ın altında kalır — göreli kapı
 // burada işe yaramaz. Risk zaten sermaye tavanıyla (mutlak $ + bütçe
 // payı) sınırlı, bu yüzden sadece "pozitif sinyal" yeterli.
            .collect();
 // teşhis: filtre BÜTÜN adayları eliyorsa en güçlü 5 oyun sayılarıyla
 // raporla (kapı eşiğine ne kadar uzakta?)
        let mut pre = cands.clone();
        pre.sort_by(|a, b| b.conviction.total_cmp(&a.conviction));
        cands.retain(|c| {
            c.conviction >= self.cfg.min_conviction
                || (c.explorer_share >= 1.0 && c.conviction > 0.0)
        });
        tracing::info!(
            "[AL] oy={} pre={} kapı_sonrası={} min={:.2} ilk3={:?}",
            n_votes_in, pre.len(), cands.len(), self.cfg.min_conviction,
            pre.iter().take(3).map(|c| format!("{}:conv={:.3} sh={:.1}", c.coin, c.conviction, c.explorer_share)).collect::<Vec<_>>()
        );
        if cands.is_empty() && !pre.is_empty() {
            tracing::info!(
                "aday yok (min_conviction={:.3}): {}",
                self.cfg.min_conviction,
                pre.iter().take(5).map(|c| format!(
                    "{} sh={:.1} w={:.3} conv={:.4}",
                    c.coin, c.explorer_share, c.kelly_weight, c.conviction
                )).collect::<Vec<_>>().join(" | ")
            );
        }

 // 2) sırala (korelasyonlu mega-bahis ilk elemede zayıf olan gider)
        cands.sort_by(|a, b| b.conviction.total_cmp(&a.conviction));

 // 3) KORELASYON FİLTRESİ: açgözlü seçim; benzer yüksek → katılmaz
        let mut picked: Vec<Candidate> = Vec::with_capacity(self.cfg.top_n);
        for c in cands {
            if picked.len() >= self.cfg.top_n {
                break;
            }
            let too_close = picked
                .iter()
                .any(|p| corr_at(&p.coin, &c.coin).abs() > self.cfg.max_correlation);
            if !too_close {
                picked.push(c);
            }
        }

 // 4) TAHSİS: conviction kare ağırlıklı (fractional Kelly dilimleri)
        let sq: Vec<f64> = picked.iter().map(|c| (c.conviction * c.conviction).max(1e-6)).collect();
        let total: f64 = sq.iter().sum();
        for (i, c) in picked.iter_mut().enumerate() {
            c.allocated_usd = budget_usd * sq[i] / total.max(1e-6);
        }
 // 5) KEŞİF SERMAYE TAVANI: bütçenin `explore_cap_frac`'i kadarını geçemez;
 // saf keşif (tek başına keşif oyu) aday ayrıca mutlak tavana tabidir.
 // Keşif havuzu borsanın $10 min notional'ına DÜŞEMEZ: bütçe $72 iken
 // %5 = $3.60 → keşif adayı her zaman elenir, şerit matematiksel olarak
 // ölü kalır.
 //
 // Taban POZİSYON BAŞINADIR: havuz iki keşifçiye bölününce her biri
 // $7.50'ye düşüyor ve ikisi de eleniyordu (Not: "2 aday $10 altı
 // tahsisle elendi"). Bu yüzden taban = keşifçi sayısı × $10.
        let n_explore = picked.iter().filter(|c| c.explorer_share > 0.0).count();
 // HAVUZ MATEMATİĞİ: iki kısıt birlikte tutarlıdır —
 // `explore_budget ≤ bütçe×%5` ve taban ≤ havuz; finanse edilebilir
 // keşifçi = min(N, ⌊havuz/$10⌋). (bütçe×%5 havuzun tamamını aşamaz;
 // taban havuzu aşamaz — 13 keşifçi × $10 = $130 gibi %5'i aşan
 // taban matematiksel olarak finansman dışıdır.)
 // Havuz asla %5'i aşmaz, taban asla havuzu aşamaz.
        let cap_usd = budget_usd * self.cfg.explore_cap_frac;
        let max_funded = if cap_usd >= MIN_NOTIONAL_USD {
            ((cap_usd / MIN_NOTIONAL_USD).floor() as usize).max(1)
        } else {
            0
        };
        if n_explore > 0 && max_funded == 0 {
            tracing::warn!(
                "[AL] keşif havuzu ${cap_usd:.2} < ${MIN_NOTIONAL_USD:.0} — bu döngü keşif şeridi KAPALI"
            );
        }
 // Havuz = %5 tavanı (sert üst sınır). Taban ($10/kişi) havuzu
 // AŞAMAZ: `max_funded` bunu zaten garanti eder, çünkü
 // max_funded = ⌊havuz/$10⌋ → havuz ≥ max_funded × $10.
        let explore_budget = cap_usd;
        let mut used = 0.0;
        let mut funded = 0usize;
        for c in picked.iter_mut() {
            if c.explorer_share <= 0.0 {
                continue;
            }
            if funded >= max_funded {
 // tavan doldu: kalan keşifçiler ELENİR (sessizce değil, loglanır)
                c.allocated_usd = 0.0;
                c.capped = true;
                tracing::info!(
                    "[AL] keşif tavanı dolu — {max_funded} kişi × ${MIN_NOTIONAL_USD:.0} = ${:.2} (havuz), artık yer yok — {} elendi",
                    max_funded as f64 * MIN_NOTIONAL_USD,
                    c.coin
                );
                continue;
            }
            let cap = if c.explorer_share >= 1.0 {
                self.cfg.explore_max_usd.min(explore_budget)
            } else {
                explore_budget
            };
            let allow = (explore_budget - used).max(0.0);
            let cap = cap.min(allow);
            if cap < MIN_NOTIONAL_USD {
                c.allocated_usd = 0.0;
                c.capped = true;
                tracing::info!("[AL] keşif {} elendi (havuz kaldı ${cap:.2})", c.coin);
                continue;
            }
            if cap < c.allocated_usd {
                c.allocated_usd = cap;
                c.capped = true;
            }
            used += c.allocated_usd;
            funded += 1;
        }
 // 5b) keşifçilere eşit taban payı: havuz N kişiye bölününce de her biri
 // $10'un üstünde kalmalı (yukarıdaki taban tek başına yetmiyor:
 // conviction farkı yüzünden biri $3 alıp eleniyordu).
 // YALNIZCA finanse edilenler arasında bölüşülür.
        let n_ex = funded.max(1);
        let min_each = explore_budget / n_ex as f64;
        if n_ex > 1 {
            for c in picked
                .iter_mut()
                .filter(|c| c.explorer_share > 0.0 && c.allocated_usd > 0.0)
            {
                if c.allocated_usd < min_each {
                    c.allocated_usd = min_each;
                    c.capped = true;
                }
            }
        }
 // 5c) POZİSYON BAŞINA TAVAN: tahsis risk kapısıyla aynı tavanı
 // aşamaz. Aksi halde tek güçlü aday TÜM bütçeyi alıyor
 // (aksi halde tek güçlü aday coin-tavanına takılır ve sinyal
 // tahsis alamadan boşa gider).
        for c in picked.iter_mut() {
            if c.allocated_usd > self.cfg.max_position_usd {
                tracing::info!(
                    "[AL] {} tahsisi ${:.0} → ${:.0} (pozisyon tavanı)",
                    c.coin,
                    c.allocated_usd,
                    self.cfg.max_position_usd
                );
                c.allocated_usd = self.cfg.max_position_usd;
                c.capped = true;
            }
        }
 // 6) MİN NOTİONAL: borsanın $10 altı emri reddi — sıfır/ mikro tahsisleri ele
        let before = picked.len();
        picked.retain(|c| c.allocated_usd >= MIN_NOTIONAL_USD);
        if before != picked.len() {
            tracing::info!("[AL] {} aday $10 altı tahsisle elendi", before - picked.len());
        }
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(v: &[(&str, f64)]) -> HashMap<String, f64> {
        v.iter().map(|(k, x)| (k.to_string(), *x)).collect()
    }

    fn vote(name: &str, dir: Dir, score: f64, voters: &[&str], atr: f64, mid: f64) -> Vote {
        Vote {
            coin: name.into(),
            dir,
            score,
            voters: voters.iter().map(|s| s.to_string()).collect(),
            weights: w(&[("Wyckoff", 1.2), ("Dow", 0.8)]),
            explore: std::collections::HashSet::new(),
            atr_pct: atr,
            mid,
        }
    }

    const CORR_OFF: fn(&str, &str) -> f64 = |_, _| 0.0;
    const CORR_ON: fn(&str, &str) -> f64 = |_, _| 0.95;

    #[test]
    fn tahsis_en_yuksek_skora_en_fazla() {
        let am = AllocationManager::new(AllocationConfig::default());
        let mut votes = vec![
            vote("BTC", Dir::Long, 0.8, &["Wyckoff", "Dow"], 0.01, 84_000.0),
            vote("SOL", Dir::Long, 0.5, &["Wyckoff", "Dow"], 0.01, 150.0),
        ];
        let picks = am.allocate(votes, &CORR_OFF, 100.0);
        assert_eq!(picks.len(), 2);
        assert!(picks[0].coin == "BTC");
        assert!(picks[0].allocated_usd > picks[1].allocated_usd);
 // R:R — long: SL altında TP üstünde
        assert!(picks[0].sl_px < picks[0].tp_px);
    }

    #[test]
    fn korelasyon_bahisini_eler() {
        let am = AllocationManager::new(AllocationConfig { max_position_usd: 1e9, ..Default::default() });
        let votes = vec![
            vote("BTC", Dir::Long, 0.9, &["Wyckoff", "Dow"], 0.01, 84_000.0),
            vote("ETH", Dir::Long, 0.7, &["Wyckoff"], 0.01, 3_000.0),
            vote("DOGE", Dir::Long, 0.5, &["Wyckoff"], 0.01, 0.2),
        ];
 // limit: BTC+ETH benzer (0.95 küme; hepsinin korele olduğu durumda)
        let picks = am.allocate(votes, &CORR_ON, 100.0);
        assert_eq!(picks.len(), 1, "tek korelasyon kümesi → yalnız en güçlü kalır");
        assert_eq!(picks[0].coin, "BTC");
        assert!((picks[0].allocated_usd - 100.0).abs() < 1e-6);
    }

    #[test]
    fn atr_risk_marji_skoru_dusturur() {
        let am = AllocationManager::new(AllocationConfig::default());
 // yönlü ağırlık = 0.6 / toplam 1.0, 5 oycu
        let calm = am.conviction(0.6, 1.0, 5, 0.01, 0.5); // sakin piyasa
        let wild = am.conviction(0.6, 1.0, 5, 0.25, 0.5); // volatil piyasa
        assert!(calm > wild, "ATR yüksek → inanç düşük: {calm} > {wild}");
        assert!(wild < 0.44);
    }

    #[test]
    fn yonsuz_oylar_ele() {
        let am = AllocationManager::new(AllocationConfig::default());
        let votes = vec![vote("X", Dir::Neutral, 0.0, &[], 0.01, 100.0)];
        let picks = am.allocate(votes, &CORR_OFF, 100.0);
        assert!(picks.is_empty());
    }

 // ── KEŞİF ŞERİDİ TESTLERİ ──────────────────────────────────────────────
    #[test]
    fn saf_kesif_tavana_tapilir() {
        let m = AllocationManager::new(AllocationConfig::default());
 // 12 kurulmuş oy → w=0.6; 3 keşif oyu → w=0.01
        let mut voters: Vec<String> = vec!["A".to_string(); 12];
        voters.extend(vec!["X".to_string(); 3]);
        let mut weights = HashMap::new();
        for _ in 0..12 { weights.insert("A".to_string(), 0.6); }
        for _ in 0..3 { weights.insert("X".to_string(), crate::track::EXPLORE_FLOOR); }
        let v = Vote {
            coin: "BTC".into(), dir: Dir::Long, score: 0.9,
            voters: voters.clone(), weights,
            explore: ["X"].iter().map(|x| x.to_string()).collect(),
            atr_pct: 0.01, mid: 100.0,
        };
        let out = m.allocate(vec![v], &|_, _| 0.0, 100_000.0);
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert!((c.explorer_share - 3.0 / 15.0).abs() < 1e-9, "keşif payı yanlış");
        assert!(c.capped, "keşif adayı tavanlanmalıydı");
 // karışık aday → mutlak keşif tavanı değil, keşif BÜTÇESİ sınıtı geçerli
        assert!(c.allocated_usd <= 100_000.0 * m.cfg.explore_cap_frac + 1e-9,
            "keşif bütçesi aşıldı: {}", c.allocated_usd);
    }

    #[test]
    fn saf_keşif_mutlak_tavana_tapilir() {
        let m = AllocationManager::new(AllocationConfig { max_position_usd: 1e9, ..Default::default() });
 // 18 keşif oyu (hepsi taban ağırlığında) → saf keşif → 150$ mutlak tavan
        let voters: Vec<String> = vec!["X".to_string(); 18];
        let v = Vote {
            coin: "BTC".into(), dir: Dir::Long, score: 0.9, voters,
            weights: HashMap::from([("X".to_string(), crate::track::EXPLORE_FLOOR)]),
            explore: std::collections::HashSet::from(["X".to_string()]),
            atr_pct: 0.01, mid: 100.0,
        };
        let out = m.allocate(vec![v], &|_, _| 0.0, 500_000.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].explorer_share, 1.0);
        assert!(out[0].capped);
        assert!((out[0].allocated_usd - m.cfg.explore_max_usd).abs() < 1e-6,
            "saf keşif 150$ tavanına oturmalı, got {}", out[0].allocated_usd);
    }

    #[test]
    fn kesif_toplam_butce_asamaz() {
        let mut cfg = AllocationConfig::default();
        cfg.max_correlation = 0.0; // hepsi geçsin
        let m = AllocationManager::new(cfg);
 // 6 saf keşif adayı → tavan yoksa bütçeyi paylaşıp aşardı
        let votes: Vec<Vote> = ["BTC", "ETH", "SOL", "XRP", "DOGE", "AVAX"]
            .iter()
            .enumerate()
            .map(|(i, c)| Vote {
                coin: c.to_string(), dir: Dir::Long, score: 0.9,
                voters: vec!["X".into()],
                weights: HashMap::from([("X".to_string(), crate::track::EXPLORE_FLOOR)]),
                explore: std::collections::HashSet::from(["X".to_string()]),
                atr_pct: 0.01 + i as f64 * 0.0001, mid: 100.0,
            })
            .collect();
        let out = m.allocate(votes, &|_, _| 0.0, 100_000.0);
        let total: f64 = out.iter().map(|c| c.allocated_usd).sum();
        assert!(total <= 100_000.0 * m.cfg.explore_cap_frac + 1e-6,
            "keşif bütçeyi aştı: {total} > {}", 100_000.0 * m.cfg.explore_cap_frac);
    }

    #[test]
    fn kesif_tabani_havuzu_asamaz() {
 // KURAL: `explore_budget ≤ bütçe×%5` ve taban ≤ havuz birlikte
 // tutarlıdır; finansedilebilir keşifçi = min(N, ⌊havuz/$10⌋)
 // → havuz asla %5'i aşmaz, taban asla havuzu aşamaz.
        let mut cfg = AllocationConfig::default();
        cfg.max_correlation = 0.0;
        let m = AllocationManager::new(cfg);
        let votes: Vec<Vote> = ["BTC", "ETH", "SOL"]
            .iter()
            .map(|c| Vote {
                coin: c.to_string(), dir: Dir::Long, score: 0.9,
                voters: vec!["X".into()],
                weights: HashMap::from([("X".to_string(), crate::track::EXPLORE_FLOOR)]),
                explore: std::collections::HashSet::from(["X".to_string()]),
                atr_pct: 0.01, mid: 100.0,
            })
            .collect();
 // bütçe $300 → havuz %5 = $15 → ⌊15/10⌋ = 1 kişi finanse edilir
        let out = m.allocate(votes, &|_, _| 0.0, 300.0);
        let total: f64 = out.iter().map(|c| c.allocated_usd).sum();
        assert!(total <= 300.0 * m.cfg.explore_cap_frac + 1e-6,
            "havuz %5'i aştı: {total}");
        for c in &out {
            assert!(c.allocated_usd >= MIN_NOTIONAL_USD - 1e-6,
                "{} $10'un altına düştü: {}", c.coin, c.allocated_usd);
        }
 // taban 3×$10 = $30 > havuz $15 → 1 kişi sadece
        assert_eq!(out.len(), 1, "havuz 1 keşifçiyi finanse edebilmeli");
    }

    #[test]
    fn kurulmus_strateji_keşif_indiriminden_etkilenmez() {
        let m = AllocationManager::new(AllocationConfig { max_position_usd: 1e9, ..Default::default() });
        let voters: Vec<String> = vec!["Elliott".to_string(); 9];
        let v = Vote {
            coin: "BTC".into(), dir: Dir::Long, score: 0.9,
            voters, weights: HashMap::from([("Elliott".to_string(), 0.667)]),
            explore: std::collections::HashSet::new(),
            atr_pct: 0.01, mid: 100.0,
        };
        let out = m.allocate(vec![v], &|_, _| 0.0, 10_000.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].explorer_share, 0.0);
        assert!(!out[0].capped, "kurulmuş strateji tavanlanmamalı");
    }

    #[test]
    fn tahsis_pozisyon_tavanini_asmaz() {
 // canlı bulgu: tek aday tüm bütçeyi ($300) aldı, risk kapısı $50 ile
 // reddetti → sinyal boşa gitti. Tahsis tavanı riski geçmemeli.
        let m = AllocationManager::new(AllocationConfig {
            max_position_usd: 50.0,
            ..Default::default()
        });
        let voters: Vec<String> = vec!["Elliott".to_string(); 9];
        let v = Vote {
            coin: "BTC".into(), dir: Dir::Long, score: 0.9,
            voters, weights: HashMap::from([("Elliott".to_string(), 0.667)]),
            explore: std::collections::HashSet::new(),
            atr_pct: 0.01, mid: 100.0,
        };
        let out = m.allocate(vec![v], &|_, _| 0.0, 300.0);
        assert_eq!(out.len(), 1);
        assert!((out[0].allocated_usd - 50.0).abs() < 1e-6, "tavan $50 uygulanmalı");
        assert!(out[0].capped);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// FEE-FARKINDALIKLI EV KAPISI (SKIP, clamp değil)
// ═══════════════════════════════════════════════════════════════════════════
//
// NEDEN VAR (sayılarla):
// round-trip maliyet ≈ 2×(taker 4.5 + slippage 5.0) = **19 bps** (%0.19)
// stop mesafesi = 1.5×ATR. Düşük volatilitede 1.5×ATR = %0.25 (ALT SINIR)
// → R = 0.25 / 0.19 ≈ **1.3** (risk 19 bps, hedef 25 bps)
// → TP = 2×SL = %0.50 → R:R = 2:1 → başabaş WR = 1/(1+2) = %33 + fee ≈ **%59**
// Yani %59'a kadar kazanma oranı **yapısal olarak negatif EV** üretir.
// Gözlenen profil: kayıplar min_dist tabanında R ≈ $0.125 ile
// stop'lanır — başabaş WR için stop mesafesi ≥ 75 bps şarttır.
//
// KURAL: `min_risk_dist_bps = max(75, 3 × round_trip_cost_bps)`
// 3×19 = 57 bps → 75 bps (%0.75) kazanır. Yani stop mesafesi en az
// **maliyetin 3 katı** olmalı; değilse işlem **SKIP** edilir (clamp değil —
// clamp stop'u daraltıp EV'yi daha da bozardı).
//
// Zarar kontrolü: SKIP bir eksiklik değil, **doğru davranış**. Düşük
// volatilite = yüksek likidite + düşük spread = aslında en iyi coinler; ama
// 1.5×ATR orada %0.25'e düşüyor. Doğru yanıt pozisyon almamak.

/// ── F — TEK MALİYET MODELİ ─────────────────────────────────────────────
/// EV kapısı ve yürütme fiyat bandı **aynı** modelden beslenir; ikinci bir
/// maliyet gerçekliği (koddaki ±%0.5 ad-hoc bandı gibi) yasaktır.
///
/// ```text
/// expected_slippage(spread) = base_slip + 0.5 × spread (IOC yarı-spread öder)
/// execution_cost(spread) = fee + expected_slippage + spread
/// round_trip(spread) = 2 × execution_cost
/// EV tabanı = max(ev_min_dist, 3 × round_trip)
/// yürütme bandı = max(execution_cost, BAND_FLOOR=50 bps)
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostModel {
 /// taker fee (bps) — Hyperliquid taker %0.045 = 4.5 bps
    pub fee_bps: f64,
 /// taban slippage (bps) — ince testnet defteri ~5 bps
    pub base_slip_bps: f64,
 /// EV tabanı (bps)
    pub ev_min_dist_bps: f64,
}

/// Yürütme fiyat bandının operasyonel tabanı (bps): ince testnet defterinde
/// model maliyeti bu değerden KÜÇÜKSE bile IOC "could not immediately match"
/// ile reddedilir. Model maliyeti daha büyükse band modelden gelir.
pub const EXECUTION_BAND_FLOOR_BPS: f64 = 50.0;

impl Default for CostModel {
    fn default() -> Self {
        Self::from_env()
    }
}

impl CostModel {
    pub fn from_env() -> Self {
        Self {
            fee_bps: env_f64("EV_TAKER_FEE_BPS", DEFAULT_TAKER_FEE_BPS),
            base_slip_bps: env_f64("EV_SLIPPAGE_BPS", DEFAULT_SLIPPAGE_BPS),
            ev_min_dist_bps: env_f64("EV_MIN_DIST_BPS", 75.0),
        }
    }

 /// Testler için deterministik kurucu (ENV'e bakmaz).
    pub fn new(fee_bps: f64, base_slip_bps: f64, ev_min_dist_bps: f64) -> Self {
        Self { fee_bps, base_slip_bps, ev_min_dist_bps }
    }

 /// **BEKLENEN KAYMA** — IOC emri dinlenen defterin yarım spread'ini de
 /// öder. `spread_bps` ≤ 0 (defter yok) → taban kayma.
    pub fn expected_slippage_bps(&self, spread_bps: f64) -> f64 {
        self.base_slip_bps + 0.5 * spread_bps.max(0.0)
    }

 /// **TEK YÖN maliyet** (bps): fee + beklenen kayma + spread.
    pub fn execution_cost_bps(&self, spread_bps: f64) -> f64 {
        self.fee_bps + self.expected_slippage_bps(spread_bps) + spread_bps.max(0.0)
    }

 /// Round-trip maliyet (bps): 2 × tek yön.
    pub fn round_trip_cost_bps(&self, spread_bps: f64) -> f64 {
        2.0 * self.execution_cost_bps(spread_bps)
    }

 /// Asgari risk mesafesi (bps): `max(EV_MIN_DIST, 3 × round_trip)`.
    pub fn min_risk_dist_bps(&self, spread_bps: f64) -> f64 {
        self.ev_min_dist_bps.max(3.0 * self.round_trip_cost_bps(spread_bps))
    }

 /// EV kapısı: `sl_mult × ATR_bps < taban` → SKIP.
 ///
 /// `atr_pct` = ATR/mid (ondalık). Dönüş: (geçti, risk_bps, taban_bps).
    pub fn ev_ok(&self, atr_pct: f64, sl_mult: f64, spread_bps: f64) -> (bool, f64, f64) {
        let atr_bps = atr_pct * 10_000.0;
        let risk_bps = sl_mult * atr_bps;
        let floor = self.min_risk_dist_bps(spread_bps);
        (risk_bps >= floor, risk_bps, floor)
    }

 /// **YÜRÜTME FİYAT BANDI** (bps, tek yön) — EV ile AYNI model:
 /// IOC limit fiyatı = referans × (1 ± band/1e4). Model maliyeti
 /// `EXECUTION_BAND_FLOOR_BPS` altındaysa operasyonel taban uygulanır
 /// (ince testnet defterinde model maliyetinin altına band daraltmak,
 /// dolum kaçışı üretirdi — Not: ).
    pub fn execution_band_bps(&self, spread_bps: f64) -> f64 {
        self.execution_cost_bps(spread_bps).max(EXECUTION_BAND_FLOOR_BPS)
    }
}

/// Varsayılanlar gözlenen gerçek değerlerdir: Hyperliquid taker 4.5 bps, ince
/// testnet defterinde gözlenen slippage ~5.0 bps.
pub const DEFAULT_TAKER_FEE_BPS: f64 = 4.5;
pub const DEFAULT_SLIPPAGE_BPS: f64 = 5.0;

fn env_f64(k: &str, d: f64) -> f64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

#[cfg(test)]
mod ev_tests {
    use super::*;

 /// F — deterministik model (ENV'e BAKMAZ; paralel test güvenli)
    fn m() -> CostModel {
        CostModel::new(4.5, 5.0, 75.0)
    }

    #[test]
    fn dusuk_volatilite_skip_uretiyor() {
 // Gözlenen profil: kayıplar 1.5×ATR ≈ %0.25'te stop'lanır.
 // Round-trip 19 bps → 25 bps stop = R 1.3 → 2:1'de başabaş WR %59.
        let atr_pct = 0.0025 / 1.5; // ATR = %0.1667 → 1.5×ATR = %0.25
        let (ok, risk_bps, floor) = m().ev_ok(atr_pct, 1.5, 0.0);
        assert!(!ok, "maliyet tabanı altında → SKIP");
        assert!((risk_bps - 25.0).abs() < 1e-6, "risk {} bps", risk_bps);
        assert!((floor - 75.0).abs() < 1e-6, "taban max(75, 3×19=57) = 75 bps");
    }

    #[test]
    fn yuksek_volatilite_gecer() {
 // ATR %1.5 → 1.5×ATR = %2.25 = 225 bps > 75 → işlem alınır
        let (ok, risk_bps, _) = m().ev_ok(0.015, 1.5, 0.0);
        assert!(ok, "225 bps > 75 bps → geçmeli");
        assert!((risk_bps - 225.0).abs() < 1e-6);
    }

    #[test]
    fn taban_maliyetin_ucun_kati() {
 // spread=0 → taban slippage 5 bps → tek yön 9.5 → rt 19
        assert!((m().round_trip_cost_bps(0.0) - 19.0).abs() < 1e-9, "2×(4.5+5.0)");
        assert!((m().min_risk_dist_bps(0.0) - 75.0).abs() < 1e-9);
    }

    #[test]
    fn spread_tek_modelden_floor_buyur() {
 // F: spread ARTIK maliyetin İÇİNDE (iki model yok):
 // slip = 5 + 0.5×30 = 20 → tek yön = 4.5+20+30 = 54.5 → rt = 109
        let cm = m();
        assert!((cm.expected_slippage_bps(30.0) - 20.0).abs() < 1e-9);
        assert!((cm.execution_cost_bps(30.0) - 54.5).abs() < 1e-9);
        assert!((cm.round_trip_cost_bps(30.0) - 109.0).abs() < 1e-9);
        let (ok, _, floor) = cm.ev_ok(0.0025 / 1.5, 1.5, 30.0);
        assert!((floor - 327.0).abs() < 1e-6, "floor = 3×109 = {floor}");
        assert!(!ok);
 // geniş spread yüksek vol'da bile işlemi kestirebilir:
 // rt = 2×(4.5+105+200) = 619 → taban 1857 → 225 bps yetmez
        let (ok2, r2, f2) = cm.ev_ok(0.015, 1.5, 200.0);
        assert!((f2 - 1857.0).abs() < 1e-6, "floor {f2}");
        assert!((r2 - 225.0).abs() < 1e-6);
        assert!(!ok2, "yayılma maliyeti yüzünden 225 bps yetmez");
    }

 /// F — EV ile yürütme bandı AYNI modelden: band = max(cost, 50 bps)
    #[test]
    fn ev_and_execution_use_same_cost_model() {
        let cm = m();
 // dar defter: operasyonel taban devrede
        assert!((cm.execution_band_bps(0.0) - 50.0).abs() < 1e-9);
 // geniş defter: band = model çıktısının BİREBİRİ (ikinci model yok)
        assert!((cm.execution_band_bps(300.0) - cm.execution_cost_bps(300.0)).abs() < 1e-9);
        assert!((cm.execution_band_bps(300.0) - 459.5).abs() < 1e-9, "4.5+155+300");
 // monotonluk: spread arttıkça hem taban hem band artar
        assert!(cm.execution_band_bps(10.0) <= cm.execution_band_bps(100.0));
        assert!(cm.min_risk_dist_bps(10.0) <= cm.min_risk_dist_bps(100.0));
 // beklenen kayma spread'siz tabana döner
        assert!((cm.expected_slippage_bps(-5.0) - 5.0).abs() < 1e-9);
    }

 /// F — yüksek beklenen kayma EV kapısını KESTİRMELİ
    #[test]
    fn high_expected_slippage_skips_trade() {
        let cm = CostModel::new(4.5, 60.0, 75.0); // kayma tabanı 60 bps
 // rt = 2×(4.5+60) = 129 → taban = 387 → 225 bps risk yetmez
        let (ok, _, floor) = cm.ev_ok(0.015, 1.5, 0.0);
        assert!((floor - 387.0).abs() < 1e-6);
        assert!(!ok, "yüksek kayma → SKIP");
    }

 /// F — yüksek spread işlemi kestirmeli
    #[test]
    fn high_spread_skips_trade() {
        let cm = m();
        let (ok, _, _) = cm.ev_ok(0.015, 1.5, 200.0);
        assert!(!ok, "spread 200 bps → taban 3×619 = 1857 → SKIP");
    }

    #[test]
    fn basabaş_wr_matematiği_kapinin_gerekcesi() {
 // SL = 25 bps, TP = 50 bps (2:1), maliyet 19 bps round-trip.
 // Kazanma getirisi net = 50 − 19 = 31 bps; kayıp = 25 + 19 = 44 bps.
 // Başabaş WR = 44 / (44+31) = %58.7 → altındaki her şey negatif EV.
        let sl = 25.0_f64;
        let tp = 50.0_f64;
        let cost = 19.0_f64;
        let win_net = tp - cost;
        let loss_net = sl + cost;
        let be = loss_net / (loss_net + win_net);
        assert!((be - 0.5868).abs() < 1e-3, "başabaş WR {be:.4}");
 // kapı 75 bps ile: SL=75, TP=150 → win_net=131, loss_net=94 → be=%41.8
        let sl2 = 75.0_f64;
        let tp2 = 150.0_f64;
        let be2 = (sl2 + cost) / (sl2 + cost + (tp2 - cost));
        assert!(be2 < be, "kapı başabaş eşiğini düşürüyor: {be2:.4} < {be:.4}");
    }

    #[test]
    fn ev_kapisi_clamp_degil_skip() {
 // Kanıt: kapı stop'u DARALTMAZ, işlemi hiç açmaz.
 // (clamp olsaydı 25 bps stop daha da yakınlaştırılır ve EV kötüleşirdi)
        let (ok, risk_bps, floor) = m().ev_ok(0.001, 1.5, 0.0); // risk 15 bps
        assert!(!ok);
        assert!((risk_bps - 15.0).abs() < 1e-6, "risk mesafesi DEĞİŞMEDİ");
        assert!(risk_bps < floor, "sadece karşılaştırma yapıldı");
    }

 // ── TP FROM ENTRY TESTLERİ ──────────────────────────────────────────
    #[test]
    fn tp_duzeltilmis_giristen_turetilir() {
        let am = AllocationManager::new(AllocationConfig::default());
 // Long: entry=100, SL=97 (risk=3), tp_atr_mult=3, sl_atr_mult=1.5
 // risk=3, tp_dist = (3/1.5)*3 = 6, min=2*3=6 → TP = 100+6 = 106
        let tp = am.tp_from_entry(Dir::Long, 100.0, 3.0);
        assert!((tp - 106.0).abs() < 1e-9, "Long TP = 106, got {tp}");

 // Short: entry=100, SL=103 (risk=3)
 // TP = 100 - 6 = 94
        let tp = am.tp_from_entry(Dir::Short, 100.0, 3.0);
        assert!((tp - 94.0).abs() < 1e-9, "Short TP = 94, got {tp}");

 // Risk mesafesi farklı: entry=50000, SL=49500 (risk=500)
 // tp_dist = (3/1.5)*500 = 1000, min=1000 → TP = 50000+1000 = 51000
        let tp = am.tp_from_entry(Dir::Long, 50000.0, 500.0);
        assert!((tp - 51000.0).abs() < 1e-9, "TP = 51000, got {tp}");
    }

    #[test]
    fn kayma_tp_kar_tarafinda_kalir() {
        let am = AllocationManager::new(AllocationConfig::default());
 // Entry kayması: planlanan entry=100, gerçek dolum=101 (Long)
 // SL=97 (risk=3 from planned), ama gerçek entry=101
 // TP = 101 + 6 = 107 (kâr tarafında)
        let tp = am.tp_from_entry(Dir::Long, 101.0, 3.0);
        assert!(tp > 101.0, "TP kâr tarafında: {tp} > 101");

 // Short: entry=100, dolum=99, SL=103 (risk=3)
 // TP = 99 - 6 = 93 < 99 (kâr tarafında)
        let tp = am.tp_from_entry(Dir::Short, 99.0, 3.0);
        assert!(tp < 99.0, "Short TP kâr tarafında: {tp} < 99");
    }

    #[test]
    fn tp_min_mesafe_iki_kati_risk() {
        let am = AllocationManager::new(AllocationConfig::default());
 // Çok küçük risk: risk=1, tp_atr_mult/sl_atr_mult = 2
 // tp_dist = 2*1 = 2, min = 2*1 = 2 → TP daima 2×risk
        let tp = am.tp_from_entry(Dir::Long, 100.0, 1.0);
        assert!((tp - 102.0).abs() < 1e-9, "TP = entry + 2×risk = 102");
    }
}


#[cfg(test)]
mod cluster_tests {
    use super::*;

    /// BAĞIMSIZLIK DÜZELTMESİ: aynı bilgi kümesinden gelen oylar genişlik
    /// ödülünü ÇOĞALTMAZ (aynı faktörün farklı formülasyonları).
    #[test]
    fn ayni_kume_genislik_odulu_cogaltmaz() {
        let am = AllocationManager::new(AllocationConfig::default());
        let mk = |coin: &str, voters: Vec<&str>| Vote {
            coin: coin.into(),
            dir: Dir::Long,
            score: 0.5,
            voters: voters.iter().map(|v| v.to_string()).collect(),
            weights: voters.iter().map(|v| (v.to_string(), 1.0)).collect(),
            explore: std::collections::HashSet::new(),
            atr_pct: 0.01,
            mid: 100.0,
        };
        // 3 oy — AYNI küme (Trend): Wyckoff, Dow, Turtle
        let ayni = am.allocate(vec![mk("BTC", vec!["Wyckoff", "Dow", "Turtle"])], &|_, _| 0.0, 300.0);
        // 3 oy — AYRI kümeler: Wyckoff (Trend), LOB (OrderFlow), Carry
        let ayri = am.allocate(vec![mk("BTC", vec!["Wyckoff", "LOB Emir Akışı", "Carry"])], &|_, _| 0.0, 300.0);
        assert_eq!(ayni[0].votes, 3);
        assert_eq!(ayri[0].votes, 3);
        // AYRI küme daha YÜKSEK conviction (bağımsız kanıt daha değerli):
        // breadth = 1 + 0.1×(küme sayısı − 1) → aynı küme 3 oy = 1.0, ayrı = 1.2
        assert!(
            ayri[0].conviction > ayni[0].conviction,
            "aynı kümeden oylar genişlik ödülü almamalı: aynı={:.4} vs ayrı={:.4}",
            ayni[0].conviction, ayri[0].conviction
        );
        // küme haritası tutarlı + DÜRÜST: 18 stratejinin hepsi eşlenmeli VE
        // haritalar dokümante edilmiş kümelerle birebir (harita bozulursa —
        // hepsi tek kümeye eşlenirse — bağımsızlık düzeltmesi etkisizleşir)
        let distinct: std::collections::HashSet<strategy::InfoCluster> =
            strategy::registry_names().iter().map(|n| strategy::info_cluster(n)).collect();
        assert!(
            distinct.len() >= 7,
            "kümeler tek tipe indirgenemez (bağımsızlık düzeltmesi etkisizleşir): {}",
            distinct.len()
        );
        assert_eq!(strategy::info_cluster("Kelly Sizing"), strategy::InfoCluster::Sizing);
        assert_eq!(strategy::info_cluster("Funding Carry"), strategy::InfoCluster::Carry);
    }
}
