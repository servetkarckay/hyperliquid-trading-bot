//! KESİT MOMENTUM — evren geneli göreli güç sıralaması.
//!
//! 1. Girdi: evren **1g** getirileri (`mark/prevDay − 1`); <20 coin → Skip.
//! 2. `rank_band` (tek kaynak — strateji + engine): üst %20 Top /
//! alt %20 BottomShort / %40-60 Mid.
//! 3. Long/Short [0.6] dilim üyeliği; orta dilim Hold.
//! 4. `rank_shift_exit_long`: LONG, Top'tan alt %40'a düştü → Exit
//! — çıkış kuralı + durumu (önceki band) STRATEJİNDE yaşar; motor
//! `exit_intent` API'siyle yalnız yönlendirir (kurallar tek yerde).
use crate::{closes, MarketInput, Signal, Strategy, ExitCtx, ExitIntent};
use math::indicators;

pub struct KesitMomentum {
    /// **Önceki döngünün sıralama bandı** (coin → band) — çıkış kuralının
    /// DURUMU stratejinin KENDİSİNDE yaşar ("kurallar tek yerde" prensibi;
    /// önceki tasarım bu haritayı engine'de tutuyordu). `Mutex` = `&self`
    /// API'siyle tekil strateji nesnesinde iç mutasyon.
    prev: std::sync::Mutex<std::collections::HashMap<String, RankBand>>,
}

impl Default for KesitMomentum {
    fn default() -> Self {
        Self { prev: std::sync::Mutex::new(std::collections::HashMap::new()) }
    }
}

/// Coinin kesit sıralamasındaki **dilimi** — tek kaynak (strateji + engine
/// aynı fonksiyonu kullanır; çıkış kuralı `exit_intent` API'siyle
/// stratejinin kontrolünde).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankBand {
 /// üst %20 (long dilimi)
    Top,
 /// %40 ile %60 arası (orta)
    Mid,
 /// alt %40 — kesit momentum LONG pozisyonu için ÇIKIŞ bölgesi
    Bottom,
 /// alt %20 (short dilimi)
    BottomShort,
}

/// Coinin sıralama dilimi. `None` = coin listede yok.
///
/// Sıralama azalan getiri; eşikler doküman kuralıdır:
/// üst %20 → `Top`, alt %20 → `BottomShort`, aradaki %40-60 bandı → `Mid`.
pub fn rank_band(universe_returns: &[(String, f64)], coin: &str) -> Option<RankBand> {
    if universe_returns.len() < 20 {
        return None;
    }
    let mut ranked = universe_returns.to_vec();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let rank = ranked.iter().position(|(c, _)| c == coin)?;
    let n = ranked.len();
    let top_k = (n as f64 * 0.2).ceil() as usize;
    let pct = rank as f64 / n as f64;
    if rank < top_k {
        Some(RankBand::Top)
    } else if rank >= n - top_k {
        Some(RankBand::BottomShort)
    } else if pct > 0.4 && pct < 0.6 {
        Some(RankBand::Mid)
    } else if pct > 0.6 {
        Some(RankBand::Bottom)
    } else {
        Some(RankBand::Mid)
    }
}

/// **DOKÜMANTE EDİLMİŞ ÇIKIŞ KURALI** (bu modülün başlığı): "üst dilimden
/// alt %40'a düşen coin Exit — otomatik çıkış kuralı".
///
/// LONG pozisyon: `Top` → (`Mid`→ hayır) → `Bottom`/`BottomShort` = ÇIKIŞ.
/// (Sıçrama da sayılır: Top'tan doğrudan alt %40'a düşmek.)
///
/// `None` prev = yerleşik kanıt yok → exit yok (false-positive yasağı).
pub fn rank_shift_exit_long(prev: Option<RankBand>, cur: Option<RankBand>) -> bool {
    matches!(prev, Some(RankBand::Top)) && matches!(cur, Some(RankBand::Bottom) | Some(RankBand::BottomShort))
}

impl Strategy for KesitMomentum {
    fn name(&self) -> &'static str {
        "Kesit Momentum"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        if input.universe_returns.len() < 20 {
            return Signal::Skip { reason: "evren getirisi < 20 coin".into() };
        }
        let mut ranked = input.universe_returns.clone();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let k = (ranked.len() as f64 * 0.2).ceil() as usize; // üst/alt %20
        let longs: Vec<&(String, f64)> = ranked.iter().take(k).collect();
        let shorts: Vec<&(String, f64)> = ranked.iter().rev().take(k).collect();

        let in_longs = longs.iter().any(|(c, _)| c == &input.coin);
        let in_shorts = shorts.iter().any(|(c, _)| c == &input.coin);

 // sıralama kayması: coin uzun listedeyken alt %40'a düşerse EXIT (doküman kuralı)
        let rank = ranked.iter().position(|(c, _)| c == &input.coin);
        let pct = rank.map(|r| r as f64 / ranked.len() as f64);

        if in_longs {
            return Signal::Long {
                strength: 0.6,
                note: format!("üst dilim (sıra {}/{}), 1g={:+.2}%", rank.unwrap() + 1, ranked.len(),
                    longs.iter().find(|(c, _)| c == &input.coin).unwrap().1 * 100.0),
            };
        }
        if in_shorts {
            let sr = shorts.iter().find(|(c, _)| c == &input.coin).unwrap().1;
            let sr_rank = ranked.iter().position(|(c, _)| c == &input.coin).unwrap_or(0) + 1;
            return Signal::Short {
                strength: 0.6,
                note: format!("alt dilim (sıra {sr_rank}/{}), 1g={:+.2}%", ranked.len(), sr * 100.0),
            };
        }
        if let Some(p) = pct {
            if p > 0.4 && p < 0.6 {
                return Signal::Hold { note: format!("orta dilim (%{:.0}) — nötr", p * 100.0) };
            }
        }
        let _ = closes(&input.candles);
        Signal::Hold { note: "dilim dışı".into() }
    }

    /// **Çıkış kuralı** (doküman: "üst dilimden alt %40'a düşen coin Exit") —
    /// durum (önceki band) DAHIL burada; motor yalnız yönlendirir.
    ///
    /// - Kural yalnız **LONG** pozisyona uygulanır (kesit momentum long'un
    ///   kapanış kuralı; short dilim bandı zaten kuralın çalışma alanı).
    /// - `position = None` (Flat) → band KAYIT EDİLMEZ → pozisyon açılışında
    ///   durum fresh başlar: girişte hemen çıkış üretilemez (false-positive
    ///   yasağı — en az bir tam döngü kanıt gerekir).
    fn exit_intent(&self, ctx: &ExitCtx) -> Option<ExitIntent> {
        let pos = ctx.position.as_ref()?;
        if !pos.is_long {
            return None;
        }
        let cur = rank_band(&ctx.universe_returns, &ctx.coin)?;
        let mut prev = self.prev.lock().expect("band kilidi");
        let prev_band = prev.insert(ctx.coin.clone(), cur);
        if rank_shift_exit_long(prev_band, Some(cur)) {
            return Some(ExitIntent {
                note: format!(
                    "kesit rank kayması (LONG): önceki={:?} → {:?}",
                    prev_band, cur
                ),
            });
        }
        None
    }
}

// momentum ölçümünde EMA eğimi desteği (referans)
#[allow(dead_code)]
fn _ref() {
    let _ = indicators::ema;
}
/// ── C — RANK KAYMASI EXIT (regression) ──────────────────────────────────
#[cfg(test)]
mod exit_tests {
    use super::*;

 /// 30 coinlik evren: coin üst %20'deyken (doküman kuralı: LONG dilimi)
 /// alt %40'a düşerse Exit KURALI tetiklenir.
    #[test]
    fn kesit_momentum_rank_kaymasinda_exit_uret() {
 // evren: i → getiri (i büyükse daha iyi)
        let universe: Vec<(String, f64)> =
            (0..30).map(|i| (format!("C{i:02}"), i as f64 / 100.0)).collect();
 // C29 = en iyi (üst %20), C14 = orta (%40-60 bandı), C01 = alt %40
        assert_eq!(rank_band(&universe, "C29"), Some(RankBand::Top));
        assert_eq!(rank_band(&universe, "C05"), Some(RankBand::BottomShort));
        let mid = rank_band(&universe, "C14");
        assert!(mid.is_some(), "orta band coin listede");

 // DOKÜMANTE KURAL: Top → (alt %40 / alt %20) = EXIT
        assert!(rank_shift_exit_long(Some(RankBand::Top), Some(RankBand::Bottom)));
        assert!(rank_shift_exit_long(Some(RankBand::Top), Some(RankBand::BottomShort)));
 // sıçrama da sayılır: Top → doğrudan Bottom
        assert!(rank_shift_exit_long(Some(RankBand::Top), rank_band(&universe, "C01")));

 // POSİTİF OLMAYAN DURUMLAR — exit üretmemeli:
 // Mid/Bottom'dan aşağı inmek zaten Top'tan değildi
        assert!(!rank_shift_exit_long(mid, Some(RankBand::Bottom)), "Mid'den düşmek Top kayması değil");
        assert!(!rank_shift_exit_long(None, Some(RankBand::Bottom)), "önceki band yok → exit YOK (false-positive yasağı)");
 // Top'ta kalıyorsa exit yok
        assert!(!rank_shift_exit_long(Some(RankBand::Top), Some(RankBand::Top)));
 // Top'tan Mid'e inmek henüz alt %40 değil → exit yok (kural: alt %40)
        assert!(!rank_shift_exit_long(Some(RankBand::Top), mid), "Top→Mid: kural alt %40 diyor");
 // SHORT bandine düşmek de alt %40'tır (long pozisyon için çıkış)
        assert!(rank_shift_exit_long(Some(RankBand::Top), Some(RankBand::BottomShort)));
 // evren < 20 → band YOK (veri yoksa karar yok)
        assert_eq!(rank_band(&universe[..10.min(universe.len())], "C29"), None);
 // listede olmayan coin
        assert_eq!(rank_band(&universe, "XX"), None);
    }

 /// Tek kaynak: `rank_band` stratejinin Long/Short dilim kararıyla TUTARLI —
 /// Top bandındaki coin strateji tarafından Long (0.6) sinyali alır.
    #[test]
    fn rank_band_strateji_oyuyla_tutarli() {
        let universe: Vec<(String, f64)> =
            (0..30).map(|i| (format!("C{i:02}"), i as f64 / 100.0)).collect();
        let input = crate::MarketInput {
            coin: "C29".into(),
            universe_returns: universe.clone(),
            ..Default::default()
        };
        match KesitMomentum::default().evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!((strength - 0.6).abs() < 1e-9);
                assert!(note.contains("üst dilim"));
                assert_eq!(rank_band(&universe, "C29"), Some(RankBand::Top));
            }
            other => panic!("üst dilim Long beklenirdi: {other:?}"),
        }
 // alt dilim → Short + band BottomShort
        let input2 = crate::MarketInput {
            coin: "C00".into(),
            universe_returns: universe.clone(),
            ..Default::default()
        };
        match KesitMomentum::default().evaluate(&input2) {
            Signal::Short { strength, .. } => {
                assert!((strength - 0.6).abs() < 1e-9);
                assert_eq!(rank_band(&universe, "C00"), Some(RankBand::BottomShort));
            }
            other => panic!("alt dilim Short beklenirdi: {other:?}"),
        }
    }
}


/// Çıkış-niyeti API'si — **durum stratejinin kendisinde** (engine'den taşındı).
#[cfg(test)]
mod exit_intent_tests {
    use super::*;
    use crate::{ExitPositionInfo, strat_test_helpers::helpers as H};
    use std::sync::Mutex;

    fn universe() -> Vec<(String, f64)> {
        (0..30).map(|i| (format!("C{i:02}"), i as f64 / 100.0)).collect()
    }

    fn ctx_long(coin: &str, u: &[(String, f64)]) -> ExitCtx {
        ExitCtx {
            coin: coin.into(),
            candles: vec![],
            universe_returns: u.to_vec(),
            position: Some(ExitPositionInfo { is_long: true, size: 1.0, entry_px: 100.0 }),
        }
    }

    /// Durum akışı: 1. döngü (Top) → niyet YOK; band KAYDEDİLDİ;
    /// 2. döngü (alt %40) → niyet VAR (kural tetiklendi). Kesit sıralaması
    /// coin'in KENDİ getirisinden geliyor → "C29" Top, "C01" alt %40.
    #[test]
    fn exit_intent_durum_stratejinin_icinde() {
        let k = KesitMomentum::default();
        let u = universe();
        // 1. döngü: Top band → önceki YOK → niyet yok (false-positive yasağı)
        assert!(k.exit_intent(&ctx_long("C29", &u)).is_none(), "ilk döngüde niyet ÜRETİLMEZ");
        // aynı coin alt %40'a düştü (getirisi sıfırın altına) → niyet VAR
        let mut u2 = universe();
        u2[29].1 = -5.0; // C29 alt %40'a düştü
        let intent = k.exit_intent(&ctx_long("C29", &u2)).expect("kayma → niyet");
        assert!(intent.note.contains("kesit rank kayması"), "{intent:?}");
        // idempotent ikinci çağrı: band zaten kayıtlı, tekrar kayma yok
        assert!(k.exit_intent(&ctx_long("C29", &u2)).is_none(), "aynı band → ikinci niyet YOK");
    }

    /// Flat (position=None) → niyet sorulmaz; band KAYIT EDİLMEZ →
    /// pozisyon açılışında durum fresh (girişte hemen çıkış ÜRETİLMEZ).
    #[test]
    fn exit_intent_flatte_niyet_ve_kayit_yok() {
        let k = KesitMomentum::default();
        let u = universe();
        let flat = ExitCtx { coin: "C29".into(), candles: vec![], universe_returns: u.clone(), position: None };
        assert!(k.exit_intent(&flat).is_none());
        // harici kontrol: harita boş kalmalı (Flat kayıt yapmaz)
        assert!(k.prev.lock().unwrap().is_empty());
        // short pozisyon → kural LONG'a özgü → niyet yok
        let short = ExitCtx {
            coin: "C29".into(),
            candles: vec![],
            universe_returns: u,
            position: Some(ExitPositionInfo { is_long: false, size: 1.0, entry_px: 100.0 }),
        };
        assert!(k.exit_intent(&short).is_none());
        assert!(k.prev.lock().unwrap().is_empty(), "short'ta band kaydı da YOK");
    }
}
