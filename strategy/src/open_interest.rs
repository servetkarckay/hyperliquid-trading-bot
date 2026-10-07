//! OPEN INTEREST — Price × OI kadrans matrisi (kanonik futures analizi).
//!
//! KANONİK MATRİS (kaynak: Coinglass "How to Judge the Market by Price,
//! OI & CVD" + klasik vadeli işlem analizi):
//!
//! | Fiyat | OI | Yorum |
//! |---|---|---|
//! | ↑ | ↑ | **yeni long açılıyor** — trend devam (Long) |
//! | ↑ | ↓ | **short kapanıyor** (short covering) — ralliye yeni para
//!   GİRMİYOR; sahte giriş YOK (Hold) |
//! | ↓ | ↑ | **yeni short açılıyor** — düşüş devam (Short) |
//! | ↓ | ↓ | **long kapanıyor/likidasyon** — düşüşe yakıt; dip teyidi
//!   YOK (Hold; kapitülasyon zamanlaması kanonik matris DEĞİL) |
//!
//! ESKİ UYDURMA KALDIRILDI: OI↓ kadransları "kontra ×0.7 Long/Short"
//! olarak işaretleniyordu — kanonik matris OI↓ = POZİSYON KAPANIŞI'dır;
//! kontra zamanlaması icattır.
//!
//! SAYI SÖZLEŞMESİ:
//! - Görece eşik: |ΔOI| / OI ≥ %0.2 — altı gürültü → Hold (oy yok).
//! - ΔOI (ctx serisi kısa) veya OI seviyesi (ctx) yoksa → Skip
//!   (HL'de ayrı likidasyon feed'i yok — OI'dan çıkarımsal; sahte
//!   kadrans yok).
//! - Güç = görece OI değişimi (rel_oi) kalibre ölçekli [0.2, 0.8].
//!
//! Deterministik, replay-idempotent, panik yok.
use crate::{closes, MarketInput, Signal, Strategy};

pub struct OpenInterest;

const REL_OI_THRESHOLD: f64 = 0.002; // %0.2 görece ΔOI — gürültü eşiği

impl Strategy for OpenInterest {
    fn name(&self) -> &'static str {
        "Open Interest"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let Some(d_oi) = input.oi_delta else {
            return Signal::Skip {
                reason: "ΔOI yok (ctx serisi kısa)".into(),
            };
        };
        let c = closes(&input.candles);
        if c.len() < 13 {
            return Signal::Skip {
                reason: ">=13 mum gerekli".into(),
            };
        }
        let d_px = c[c.len() - 1] - c[c.len() - 13];
        // GÖRECE EŞİK: mutlak ΔOI tek başına anlamsız — seviyeye göre
        // ölçekle (|ΔOI| / OI). Eşik altı = gürültü → Hold (oy yok).
        // OI seviyesi yoksa görece ölçek hesaplanamaz → Skip.
        let Some(ctx) = &input.ctx else {
            return Signal::Skip {
                reason: "OI seviyesi yok — görece ölçek hesaplanamaz".into(),
            };
        };
        let rel_oi = d_oi.abs() / ctx.open_interest.max(1e-12);
        // NaN/non-finite → Hold (orijinal `!(x > t)` ile birebir aynı)
        if !rel_oi.is_finite() || rel_oi <= REL_OI_THRESHOLD {
            return Signal::Hold {
                note: format!(
                    "ΔOI OI seviyesinin %{:.2}'sinin altında — gürültü, oy yok",
                    rel_oi * 100.0
                ),
            };
        }
        let oi_up = d_oi > 0.0;
        let px_up = d_px > 0.0;
        // GÜÇ: görece OI değişiminden kalibre ölçek (birim düzeltmesi:
        // eski |ΔOI|/px formülü birim olarak anlamsızdı)
        let strength = (rel_oi * 20.0).clamp(0.2, 0.8);

        match (oi_up, px_up) {
            (true, true) => Signal::Long {
                strength,
                note: format!("OI↑ + P↑: yeni long açılışı — trend devam (görece ΔOI %{:.2})", rel_oi * 100.0),
            },
            (true, false) => Signal::Short {
                strength,
                note: format!("OI↑ + P↓: yeni short açılışı — düşüş devam (görece ΔOI %{:.2})", rel_oi * 100.0),
            },
            (false, true) => Signal::Hold {
                note: "OI↓ + P↑: short covering — ralliye yeni para girmiyor; giriş kanonik değil".into(),
            },
            (false, false) => Signal::Hold {
                note: "OI↓ + P↓: long kapanışı/likidasyon — dip teyidi kanonik matriste yok; giriş üretilmez".into(),
            },
        }
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;

    fn ctx_oi(oi: f64) -> data::market_ctx::CtxState {
        data::market_ctx::CtxState {
            coin: "T".into(),
            funding: 0.0,
            open_interest: oi,
            mark_px: 100.0,
            oracle_px: 100.0,
            premium: 0.0,
            day_ntl_vlm: 1e6,
            prev_day_px: 100.0,
            ts: 0,
        }
    }

    fn mk(candles: Vec<data::Candle>, d: f64, oi: f64) -> MarketInput {
        MarketInput {
            coin: "T".into(),
            candles,
            oi_delta: Some(d),
            ctx: Some(ctx_oi(oi)),
            ..Default::default()
        }
    }

    /// A — OI↑ + P↑ → Long (yeni long, trend devam).
    #[test]
    fn oi_up_px_up_long() {
        let candles = H::linear(20, 100.0, 0.5);
        // ΔOI=2.0 / OI=400 → görece +%0.5 (eşik %0.2 üstü) → oy
        let input = mk(candles, 2.0, 400.0);
        match OpenInterest.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!((0.2..=0.8).contains(&strength));
                assert!(note.contains("yeni long"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

    /// B — OI↑ + P↓ → Short (yeni short, düşüş devam).
    #[test]
    fn oi_up_px_down_short() {
        let down = H::linear(20, 110.0, -0.5);
        let input = mk(down, 2.0, 400.0);
        assert!(
            matches!(OpenInterest.evaluate(&input), Signal::Short { strength, .. } if strength >= 0.2)
        );
    }

    /// C — KANONİK MATRİS: OI↓ kadransları KONTRA GİRİŞ ÜRETMEZ
    /// (eski ×0.7 kontra Long/Short uydurması kaldırıldı — kanonik
    /// matris OI↓ = pozisyon kapanışı, giriş kanonik değil).
    #[test]
    fn oi_down_kadranslari_kontra_giris_uretmez() {
        // OI↓ + P↑ → short covering → Hold (kontra Short YOK)
        let up = H::linear(20, 100.0, 0.5);
        let r1 = OpenInterest.evaluate(&mk(up, -2.0, 400.0));
        match &r1 {
            Signal::Hold { note } => assert!(note.contains("short covering"), "{note}"),
            other => panic!("OI↓+P↑ Hold beklenirdi: {other:?}"),
        }
        // OI↓ + P↓ → long likidasyon → Hold (kontra Long YOK)
        let down = H::linear(20, 110.0, -0.5);
        let r2 = OpenInterest.evaluate(&mk(down, -2.0, 400.0));
        match &r2 {
            Signal::Hold { note } => assert!(note.contains("long kapanışı"), "{note}"),
            other => panic!("OI↓+P↓ Hold beklenirdi: {other:?}"),
        }
    }

    /// D — ΔOI yoksa Skip; OI seviyesi yoksa Skip (sahte kadrans yasağı).
    #[test]
    fn oi_veri_yoksa_skip() {
        let input = MarketInput {
            coin: "T".into(),
            candles: H::flat(20, 100.0),
            oi_delta: None,
            ..Default::default()
        };
        assert!(matches!(OpenInterest.evaluate(&input), Signal::Skip { .. }));
        let no_lvl = MarketInput {
            coin: "T".into(),
            candles: H::flat(20, 100.0),
            oi_delta: Some(1.0),
            ..Default::default()
        };
        assert!(
            matches!(OpenInterest.evaluate(&no_lvl), Signal::Skip { .. }),
            "OI seviyesi yok → Skip"
        );
    }

    /// GÖRECE EŞİK: pencerenin %0.2'sinin altındaki ΔOI = gürültü → oy YOK.
    #[test]
    fn kucuk_delta_oy_veremez() {
        // ΔOI=0.5 / OI=4000 → +%0.0125 (eşik %0.2 altı) → Hold
        let gurultu = mk(H::linear(20, 100.0, 0.5), 0.5, 4000.0);
        match OpenInterest.evaluate(&gurultu) {
            Signal::Hold { note } => assert!(note.contains("gürültü"), "{note}"),
            other => panic!("gürültü ΔOI oy veremez (Hold beklenirdi): {other:?}"),
        }
    }

    /// IDEMPOTENT: aynı seri → aynı sinyal.
    #[test]
    fn ayni_seri_ayni_sinyal() {
        let c = H::linear(20, 100.0, 0.5);
        let i1 = mk(c.clone(), 2.0, 400.0);
        let i2 = mk(c, 2.0, 400.0);
        assert_eq!(OpenInterest.evaluate(&i1), OpenInterest.evaluate(&i2));
    }
}
