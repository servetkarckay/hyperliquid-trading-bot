//! ENGINE — ticaret motoru: karar → emir → pozisyon → öğrenme.
//!
//! Modüller:
//! - `engine_loop`       — ana döngü (tarama→tahsis→yürütme→çıkışlar)
//! - `executor`          — borsa I/O ince katmanı (SL/TP, cloid, recon)
//! - `allocation_manager`— sermaye tahsisi + TEK maliyet modeli
//! - `state_machine`     — pozisyon durum makinesi (idempotent)
//! - `risk_manager`      — emir öncesi risk kapıları
//! - `track`             — canlı öğrenme / Kelly / metrikler
//! - `orderbook`         — yerel L2 defter
//! Kural: motor beslenmeden strateji çalışmaz; borsa yanıtı UNKNOWN
//! olursa reconciliation yapılmadan state değişmez.
pub mod engine_loop;
pub mod epoch;
pub mod executor;
pub mod allocation_manager;
pub mod state_machine;
pub mod orderbook;
pub mod risk_manager;
pub mod track;

pub use executor::{Executor, OrderInfo, Position};
pub use state_machine::{Action, BotOrder, Decision, Machines, PosState, PositionStateMachine, Side, SmConfig};
pub use orderbook::{LevelF, LocalBook};
pub use risk_manager::{RiskConfig, RiskManager, RiskViolation};
