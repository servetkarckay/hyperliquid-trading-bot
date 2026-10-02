//! # engine
//!
//! Emir motoru: veri katmanından gelen gerçek veriyle pozisyon yönetimi.
//! - orderbook.rs    : yerel L2 defter (WS snapshot senkronu + türetilmiş metrikler)
//! - executor.rs     : api Exchange üstünde emir/iptal + doldurma takibi
//! - risk_manager.rs : tüm emirlerden önce risk kapısı (limitler, drawdown)
//! Bağımlılıklar: hyperliquid-api (veri erişimi), utils (saat/log).
pub mod engine_loop;
pub mod executor;
pub mod backtest;
pub mod backtest_multi;
pub mod allocation_manager;
pub mod state_machine;
pub mod orderbook;
pub mod risk_manager;
pub mod track;

pub use executor::{Executor, OrderInfo, Position};
pub use state_machine::{Action, BotOrder, Decision, Machines, PosState, PositionStateMachine, Side, SmConfig};
pub use orderbook::{LevelF, LocalBook};
pub use risk_manager::{RiskConfig, RiskManager, RiskViolation};
