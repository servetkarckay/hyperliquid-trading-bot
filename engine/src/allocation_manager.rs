//! SERMAYE TAHSİS MOTORU — "sinyal geldiyse gir" değil, "evreni tara,
//! sırala, birbirine benzeyenleri ele, en iyi ve en ÇEŞİTLİ N coine gir,
//! en güçlü olana en çok para ver".
//!
//!wall Street portföy optimizasyonu katmanı:
//!   1. İNANÇ SKORU (conviction):
//!      oy yoğunluğu × dinamik-Kelly ağırlığı ÷ ATR risk marjı
//!   2. KORELASYON FİLTRESİ (çeşitlendirme zorunluluğu):
//!      seçilmiş adaylar arasındaki korelasyon > %70 ise aday düşer
//!      (mega bahis denetimi — borsa çöküşü koruması)
//!   3. TAHSİS: conviction-temelli oranlı tahsis (fractional Kelly dilimleri)

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
    /// ATR tabanlı SL/TP (mid'e göre; long: −sl/+tp)
    pub sl_px: f64,
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
}

impl Default for AllocationConfig {
    fn default() -> Self {
        Self {
            top_n: 5,
            max_correlation: 0.70,
            sl_atr_mult: 1.5,
            tp_atr_mult: 3.0,
            // Ölçek düzeltmesi (kelly büyüklüğü) sonrası yeniden kalibre:
            // Elliott öncülü 3 oy → 0.039; zayıf karışık oy ~0.001, keşif oyu
            // ayrı baypas. 0.05 eski pay ölçeğine göre ayarlanmıştı.
            // Ağırlık-payı ölçeğiyle: %12+ sermaye yönlü (güç ve volatilite
            // indirimi sonrası). Tek zayıf oy ~0.005, keşif oyu ayrı baypas.
            min_conviction: 0.12,
            votes_norm: 18,
            max_position_usd: 50.0,
            explore_max_usd: 150.0,
            explore_cap_frac: 0.05,
            explore_discount: 0.7,
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
        // ensemble sermayesinin ne kadarı bu yöne oy veriyor (0..1)
        let share = (w_sum / w_total.max(1e-9)).clamp(0.0, 1.0);
        // kaç strateji katılıyor: tek güçlü oy = 1.0, 6+ oyuncu = 1.5 (tavan)
        let breadth = (1.0 + 0.1 * (n_voters as f64 - 1.0)).clamp(0.5, 1.5);
        let strength = 0.6 + 0.4 * score_abs.clamp(0.0, 1.0);
        let risk = 1.0 + atr_pct * 20.0; // %5 ATR ≈ 2× marj → yarıma düşer
        (share * breadth * strength / risk).clamp(0.0, 1.0)
    }

    /// SL/TP fiyatları: long → mid−slA·ATR / mid+tpA·ATR (ayna short)
    pub fn sl_tp(&self, dir: Dir, mid: f64, atr_usd: f64) -> (f64, f64) {
        match dir {
            Dir::Long => (
                mid - self.cfg.sl_atr_mult * atr_usd,
                mid + self.cfg.tp_atr_mult * atr_usd,
            ),
            Dir::Short => (
                mid + self.cfg.sl_atr_mult * atr_usd,
                mid - self.cfg.tp_atr_mult * atr_usd,
            ),
            Dir::Neutral => (mid, mid),
        }
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
                let n_voters = v.voters.len();
                let mut kw_sum = 0.0;
                let mut n_explore = 0usize;
                for n in &v.voters {
                    let w = v.weights.get(n).copied().unwrap_or(0.0);
                    if v.explore.contains(n) {
                        n_explore += 1;
                    }
                    kw_sum += w;
                }
                let kw = if n_voters == 0 { 0.0 } else { kw_sum / n_voters as f64 };
                let explorer_share = n_explore as f64 / n_voters.max(1) as f64;
                let w_total: f64 = v.weights.values().sum::<f64>();
                let mut conv =
                    self.conviction(kw_sum, w_total, n_voters, v.atr_pct, v.score.abs());
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
        //    saf keşif (tek başına keşif oyu) aday ayrıca mutlak tavana tabidir.
        // Keşif havuzu borsanın $10 min notional'ına DÜŞEMEZ: bütçe $72 iken
        // %5 = $3.60 → keşif adayı her zaman elenir, şerit matematiksel olarak
        // ölü kalır.
        //
        // Taban POZİSYON BAŞINADIR: havuz iki keşifçiye bölününce her biri
        // $7.50'ye düşüyor ve ikisi de eleniyordu (ölçüldü: "2 aday $10 altı
        // tahsisle elendi"). Bu yüzden taban = keşifçi sayısı × $10.
        let n_explore = picked.iter().filter(|c| c.explorer_share > 0.0).count();
        // HAVUZ MATEMATİĞİ (çelişki çözüldü): eskiden
        // `explore_budget = max(bütçe×%5, n_keşifçi×$10)` idi — iki kısıt
        // matematiksel olarak ÇELİŞİYORDU: 13 keşifçi → taban $130 > hesabın
        // tamamı; küçük hesapta %5 = $5.85 < $10 tabanı. Doğrusu:
        //   finanse edilebilir keşifçi = min(N, ⌊havuz/$10⌋)
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
        //     $10'un üstünde kalmalı (yukarıdaki taban tek başına yetmiyor:
        //     conviction farkı yüzünden biri $3 alıp eleniyordu).
        //     YALNIZCA finanse edilenler arasında bölüşülür.
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
        // (ölçüldü: bütçe $300, tek aday → $300 tahsis → "coin toplamı
        // 299.99 USD > 50.00" ile reddedildi, sinyal boşa gitti).
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
        // ÇELİŞKİ KURALI: eskiden `max(bütçe×%5, n_keşifçi×$10)` idi —
        // 13 keşifçi → taban $130, küçük hesapta %5 = $5.85 < $10. İkisi
        // matematiksel olarak çelişiyordu. Doğrusu:
        //   finansedilebilir keşifçi = min(N, ⌊havuz/$10⌋)
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
