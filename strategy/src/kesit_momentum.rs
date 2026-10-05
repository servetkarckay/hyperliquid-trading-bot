//! KESİT MOMENTUM — evren geneli göreli güç sıralaması.
//!
//! AMAÇ: tekil coin trendi değil, TÜM coinler arasındaki göreli sıra.
//!
//! # PENCERE: **1 GÜN** (P1-G ölçümü)
//!
//! `MarketInput::universe_returns` motor tarafında şöyle kurulur
//! (`engine_loop.rs`):
//! ```ignore
//! ctx.all().filter(|c| c.prev_day_px > 0 && c.mark_px > 0)
//!     .map(|c| (c.coin, c.mark_px / c.prev_day_px - 1.0))
//! ```
//! Yani **günlük getiri** (`activeAssetCtx.prevDayPx` → `markPx`). Kod tabanında
//! 7 günlük getiri hesabı **YOKTUR** (doğrulandı: `prev_day_px` dışında başka
//! bir alan kullanılmıyor). Dokümanın "7g sıralama" ifadesi **kodla
//! çelişiyordu** ve düzeltildi.
//!
//! HESAPLAMA:
//! 1. Evren 1g getirileri azalan sıralanır
//! 2. Üst %20 = long dilim, alt %20 = short dilim
//! 3. Coinin sırada bulunması Long/Short [0.6]; orta bar (0.4-0.6) nötr
//! 4. Sıralama kayması: üst dilimden alt %40'a düşen coin Exit —
//!    otomatik çıkış kuralı
//! GEREKLİ VERİ: >=20 coin evren getirisi (data pipeline besler).
//! NOT: kripto kesit korelasyonu yüksek (ortak beta) — risk katmanı
//! kovaryans düzeltmesini uygulamalı.
use crate::{closes, MarketInput, Signal, Strategy};
use math::indicators;

pub struct KesitMomentum;

/// Coinin kesit sıralamasındaki **dilimi** — tek kaynak (strateji + engine
/// aynı fonksiyonu kullanır; engine pozisyon tarafını bilerek exit yönlendirir).
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
        match KesitMomentum.evaluate(&input) {
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
        match KesitMomentum.evaluate(&input2) {
            Signal::Short { strength, .. } => {
                assert!((strength - 0.6).abs() < 1e-9);
                assert_eq!(rank_band(&universe, "C00"), Some(RankBand::BottomShort));
            }
            other => panic!("alt dilim Short beklenirdi: {other:?}"),
        }
    }
}
