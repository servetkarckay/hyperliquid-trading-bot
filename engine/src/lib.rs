//! ENGINE — ticaret motoru: karar → emir → pozisyon → öğrenme.
//!
//! Modüller:
//! `engine_loop` — ana döngü (tarama→tahsis→yürütme→çıkışlar)
//! `executor` — borsa I/O ince katmanı (SL/TP, cloid, recon)
//! `allocation_manager`— sermaye tahsisi + TEK maliyet modeli
//! `state_machine` — pozisyon durum makinesi (idempotent)
//! `risk_manager` — emir öncesi risk kapıları
//! `track` — canlı öğrenme / Kelly / metrikler
//! `orderbook` — yerel L2 defter
//! Kural: motor beslenmeden strateji çalışmaz; borsa yanıtı UNKNOWN
//! olursa reconciliation yapılmadan state değişmez.
pub mod allocation_manager;
pub mod engine_loop;
pub mod epoch;
pub mod executor;
pub mod orderbook;
pub mod risk_manager;
pub mod state_machine;
pub mod track;

pub use executor::{Executor, OrderInfo, Position};
pub use orderbook::{LevelF, LocalBook};
pub use risk_manager::{RiskConfig, RiskManager, RiskViolation};
pub use state_machine::{
    Action, BotOrder, Decision, Machines, PosState, PositionStateMachine, Side, SmConfig,
};
#[cfg(test)]
pub mod helpers_for_tests {
    /// Test mumu (kapalı)
    pub fn candle(t: i64, o: f64, h: f64, l: f64, c: f64, v: f64) -> data::Candle {
        data::Candle {
            t,
            o,
            h,
            l,
            c,
            v,
            n: 1,
            closed: true,
        }
    }
    pub fn flat(n: usize, px: f64) -> Vec<data::Candle> {
        (0..n)
            .map(|i| candle(i as i64 * 3_600_000, px, px * 1.001, px * 0.999, px, 100.0))
            .collect()
    }
}

#[cfg(test)]
mod e2e_matrix;
