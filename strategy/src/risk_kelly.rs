//! RİSK KELLY — işlem BOYUTU hesabı (yön üretmez).
//!
//! FORMÜL:
//!   f* = p - (1-p)/b  (binary Kelly; p=win rate, b=win/loss oranı)
//!   pragmatik = f*·0.5 (yarım Kelly — tüm sistemin risk tabanı)
//!   maks pozisyon = equity · yarım-Kelly
//! Örnek istatistik (%45, 1.8:1) — canlıda Executor fill'lerinden
//! gerçek win/loss istatistikleri beslenecek. Sinyal: Hold + boyut
//! tahmini notta. ATR-birim boyut Turtle stratejisindedir.
//! GEREKLİ VERİ: equity.
use crate::{MarketInput, Signal, Strategy};
use math::portfolio;

pub struct RiskKelly;

impl Strategy for RiskKelly {
    fn name(&self) -> &'static str {
        "Kelly Sizing"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        if input.equity <= 0.0 {
            return Signal::Skip { reason: "equity yok".into() };
        }
        // ATR ile birim boyut: 1 birim risk = %1 equity / ATR$
        // (gerçek win-rate istatistikleri executor fill'lerinden beslenir;
        //  örnek varsayım: %45 kazanma, 1.8:1 k/z — canlıda executor'dan)
        let (win_rate, wl_ratio) = (0.45, 1.8);
        let kelly = portfolio::kelly_binary(win_rate, wl_ratio).unwrap_or(0.0);
        // yarım Kelly (pratik kural)
        let half = kelly * 0.5;
        let max_usd = input.equity * half;

        Signal::Hold {
            note: format!(
                "Kelly f*={kelly:.3} -> yarım Kelly {half:.3} -> maks pozisyon ${max_usd:.0} (equity ${:.0})",
                input.equity
            ),
        }
    }
}
