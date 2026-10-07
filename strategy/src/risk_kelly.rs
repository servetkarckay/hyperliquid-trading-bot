//! RISK KELLY — Kelly kriteri işlem boyutu hesabı (yön üretmez,
//! AUXILIARY).
//!
//! KANONİK KAYNAKLAR:
//! - **Kelly (1956), "A New Interpretation of Information Rate"** —
//!   orijinal: büyüme oranını maksimize eden oran (log-utility).
//! - **Thorp — binary form**: g(f) = p·log(1+f) + q·log(1−f);
//!   even-money'de f* = p − q; odds b'de **f* = p − (1−p)/b** — bu
//!   modülün formülü.
//! - **Yarım Kelly** — Thorp: "pratikte genel olarak aşağı doğru
//!   değiştirilir" (varyans düşürme); kod: f*/2 (kanonik pratik).
//!
//! VERİ SÖZLEŞMESİ: p ve b YALNIZ tracker'ın GERÇEK canlı kanıtından
//! (`MarketInput::portfolio_*`); kanıt yok → Skip — sabit win-rate
//! (örn. %45) ile boyutlama YASAKTIR (kaynak-tarama testli).
//! f* ≤ 0 → pozitif edge yok → pozisyon $0.
//!
//! KAPSAM: **PORTFÖY-DÜZEY advisory** — coin/strateji-özel Kelly
//! DEĞİLDİR; tracker'ın toplam canlı kanıt pencerisinden okunur.
//! Çıktı Hold/Skip — actual allocation'a etkisi SIFIR (testle
//! kanıtlı); not yalnız boyut ÖNERİSİdir.
use crate::{MarketInput, Signal, StratStatus, Strategy};
use math::portfolio;

pub struct RiskKelly;

impl Strategy for RiskKelly {
    fn name(&self) -> &'static str {
        "Kelly Sizing"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        if input.equity <= 0.0 {
            return Signal::Skip {
                reason: "equity yok".into(),
            };
        }
        let (Some(p), Some(b)) = (input.portfolio_win_rate, input.portfolio_wl_ratio) else {
            // Sabit win-rate YOK: kanıt olmadan Kelly sayısı uydurulmaz.
            return Signal::Skip {
                reason: "canlı Kelly istatistiği yok (tracker kanıtı boş) — sabit %45 KULLANILMADI"
                    .into(),
            };
        };
        let kelly = portfolio::kelly_binary(p, b).unwrap_or(0.0);
        // KANONİK: f* ≤ 0 = pozitif edge YOK → Kelly 0 önerir (yatırım yok)
        if kelly <= 0.0 {
            return Signal::Hold {
                note: format!(
                    "Kelly f*={kelly:.3} ≤ 0 — pozitif edge YOK (p={p:.2} b={b:.2}); pozisyon $0"
                ),
            };
        }
        // yarım Kelly (pratik kural — Thorp: pratikte aşağı doğru ayarlanır)
        let half = kelly * 0.5;
        let max_usd = input.equity * half;

        Signal::Hold {
            note: format!(
                "Kelly f*={kelly:.3} (canlı p={p:.2} b={b:.2}) -> yarım Kelly {half:.3} -> maks pozisyon ${max_usd:.0} (equity ${:.0})",
                input.equity
            ),
        }
    }

    /// Boyut ÖNERİSİ üretir, yönlü giriş sinyali üretmez → `AUXILIARY`.
    fn status(&self, _input: &MarketInput) -> StratStatus {
        StratStatus::Auxiliary
    }
}
/// ── E — KELLY GERÇEK VERİ (sabit %45 yasağı) ───────────────────────────
#[cfg(test)]
mod kelly_tests {
    use super::*;
    use crate::ensemble::{Dir, Ensemble, EnsembleConfig};

    /// Sabit win-rate ile canlı sizing YAPILMAZ: kanıt yoksa Skip +
    /// notta "KULLANILMADI" yazısı (sabit %45 gibi varsayılan, örnek
    /// istatistiği canlı kanıt gibi gösteremez).
    #[test]
    fn kelly_does_not_use_hardcoded_live_win_rate() {
        let input = MarketInput {
            equity: 300.0,
            portfolio_win_rate: None,
            portfolio_wl_ratio: None,
            ..Default::default()
        };
        match RiskKelly.evaluate(&input) {
            Signal::Skip { reason } => {
                assert!(
                    reason.contains("KULLANILMADI"),
                    "yasağı belgelemeli: {reason}"
                );
                // sabit değerin KALDIRILDIĞININ kanıtı: kaynakta 0.45 yok
                // yalnız ÜRETİM kodu taranır (test modülünün kendi kalıpları değil)
                let src = include_str!("risk_kelly.rs")
                    .split("#[cfg(test)]")
                    .next()
                    .unwrap_or("");
                for (i, l) in src.lines().enumerate() {
                    let t = l.trim_start();
                    if t.starts_with("//") || t.starts_with("///") {
                        continue;
                    }
                    assert!(
                        !t.contains("0.45"),
                        "satır {}: sabit win-rate KAYNAKTA YER ALMAMALI: {l}",
                        i + 1
                    );
                    assert!(
                        !t.contains("1.8") && !t.contains("(0.45"),
                        "satır {}: sabit wl_ratio KAYNAKTA YER ALMAMALI: {l}",
                        i + 1
                    );
                }
            }
            other => panic!("kanıt yokken Skip beklenirdi: {other:?}"),
        }
    }

    /// GERÇEK tracker kanıtı geldiğinde not o veriyi TAŞIR (uydurma değil).
    #[test]
    fn kelly_not_dan_gelen_gercek_istatistigi_kullanir() {
        let input = MarketInput {
            equity: 300.0,
            portfolio_win_rate: Some(0.55),
            portfolio_wl_ratio: Some(1.35),
            ..Default::default()
        };
        match RiskKelly.evaluate(&input) {
            Signal::Hold { note } => {
                // not GERÇEK p ve b'yi gösterir (sabit %45 DEĞİL)
                assert!(note.contains("p=0.55"), "gerçek win-rate: {note}");
                assert!(note.contains("b=1.35"), "gerçek k/z: {note}");
                assert!(!note.contains("0.45"), "sabit değer izleri YOK: {note}");
                // f* = 0.55 - 0.45/1.35 = 0.2167 → yarı = 0.1083 → $32.5
                let f = math::portfolio::kelly_binary(0.55, 1.35).unwrap();
                assert!((f - (0.55 - 0.45 / 1.35)).abs() < 1e-12);
                assert!(note.contains(&format!("${:.0}", 300.0 * f * 0.5)));
            }
            other => panic!("kanıt varken Hold beklenirdi: {other:?}"),
        }
    }

    /// Kelly çıktısı H âlde Hold/Skip → ACTUAL ALLOCATION'A ETKİSİ SIFIR:
    /// ensemble skoruna katkı vermez (auxiliary, yönlü sinyal yok).
    #[test]
    fn kelly_allocationa_etki_etmez() {
        let e = Ensemble::new(EnsembleConfig::default());
        let cs = e.combine(vec![(
            "Kelly Sizing".into(),
            RiskKelly.evaluate(&MarketInput {
                equity: 300.0,
                portfolio_win_rate: Some(0.55),
                portfolio_wl_ratio: Some(1.35),
                ..Default::default()
            }),
        )]);
        assert!(
            matches!(cs.dir, Dir::Neutral),
            "Kelly tek başına giriş KARARI üretmez"
        );
        assert!(
            cs.contributors.is_empty(),
            "Kelly skora KATKI YAZMAZ: {cs:?}"
        );
        // durum: AUXILIARY
        assert_eq!(
            RiskKelly.status(&MarketInput::default()),
            StratStatus::Auxiliary
        );
        // Market Making de AUXILIARY (yönlü sinyal üretmez)
        use crate::market_making::MarketMaking;
        assert_eq!(
            MarketMaking.status(&MarketInput::default()),
            StratStatus::Auxiliary
        );
    }
}

/// f* ≤ 0 → pozitif edge YOK → pozisyon $0 (kanonik Kelly sıfır önerir).
#[test]
fn kelly_sifir_edge_pozisyon_sifir() {
    let input = MarketInput {
        equity: 300.0,
        portfolio_win_rate: Some(0.5),
        portfolio_wl_ratio: Some(1.0), // b=1: f* = p − (1−p) = 0
        ..Default::default()
    };
    match RiskKelly.evaluate(&input) {
        Signal::Hold { note } => {
            assert!(note.contains("pozitif edge YOK"), "{note}");
            assert!(note.contains("$0"), "{note}");
        }
        other => panic!("Hold beklenirdi: {other:?}"),
    }
}
