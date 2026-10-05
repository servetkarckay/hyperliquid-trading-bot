//! RISK MANAGER — emir öncesi risk kapıları (pre_trade).
//!
//! Kapılar: pozisyon/toplam notional tavanı, kaldıraç, minimum equity,
//! max açık pozisyon, drawdown limiti. `is_halted` yalnız operatörle
//! `resume` olur (otomatik açılış yok). İhlal → `RiskViolation` (emir
//! gönderilmez); kapı EV/tahsis kapısından BAĞIMSIZdır (savunma derinliği).
use std::collections::HashMap;

use crate::executor::Position;

#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// Tek coin maksimum pozisyon değeri (USD)
    pub max_position_notional_usd: f64,
    /// Toplam maruziyet üst sınırı (USD)
    pub max_total_notional_usd: f64,
    /// Hesap kaldıraç üst sınırı
    pub max_leverage: f64,
    /// Hesap equity'si bunun altına inerse yeni emir yok
    pub min_account_equity_usd: f64,
    /// Hyperliquid min emri ~$10; altındaki emir API'de reddedilir (girdi önce ele)
    pub min_order_notional_usd: f64,
    /// pozisyon başına asgari sermaye → dinamik pozisyon sayısı = equity / bu
    pub equity_per_position_usd: f64,
    /// Peak equity'den bu kadar düşüş -> trading durdur
    pub max_drawdown_pct: f64,
    /// Coin başına açık emir sayısı üst sınırı
    pub max_open_orders_per_coin: usize,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_position_notional_usd: 5_000.0,
            max_total_notional_usd: 20_000.0,
            max_leverage: 5.0,
            min_account_equity_usd: 50.0,
            min_order_notional_usd: crate::allocation_manager::MIN_NOTIONAL_USD,
            equity_per_position_usd: 15.0,
            max_drawdown_pct: 20.0,
            max_open_orders_per_coin: 10,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum RiskViolation {
    #[error("trading durduruldu: {0}")]
    Halted(String),
    #[error("equity taban altında: {0:.2} USD")]
    EquityFloor(f64),
    #[error("drawdown limiti aşıldı: {0:.2}%")]
    Drawdown(f64),
    #[error("pozisyon limiti: coin toplamı {0:.2} USD > {1:.2}")]
    PositionLimit(f64, f64),
    #[error("toplam maruziyet {0:.2} USD > {1:.2}")]
    TotalExposure(f64, f64),
    #[error("kaldıraç {0:.1}x > limit {1:.1}x")]
    Leverage(f64, f64),
    #[error("coin için açık emir sayısı {0} > {1}")]
    OpenOrders(usize, usize),
    #[error("emir çok küçük: {0:.2} USD < min notional")]
    TooSmall(f64),
}

pub struct RiskManager {
    pub cfg: RiskConfig,
    peak_equity: f64,
    current_equity: f64,
    halted: Option<String>,
}

impl RiskManager {
    pub fn new(cfg: RiskConfig) -> Self {
        Self { cfg, peak_equity: 0.0, current_equity: 0.0, halted: None }
    }

    pub fn config(&self) -> &RiskConfig {
        &self.cfg
    }

    /// Her equity güncellemesinde çağır: peak takibi + drawdown durdurma
    pub fn update_equity(&mut self, equity: f64) {
        self.current_equity = equity;
        if equity > self.peak_equity {
            self.peak_equity = equity;
        }
        if self.peak_equity > 0.0 {
            let dd = self.drawdown_pct(equity);
            if dd >= self.cfg.max_drawdown_pct && self.halted.is_none() {
                let msg = format!("drawdown %{dd:.2} (peak={:.2}, now={equity:.2})", self.peak_equity);
                tracing::error!("{msg} — TRADING DURDURULDU");
                self.halted = Some(msg);
            }
        }
    }

    pub fn drawdown_pct(&self, equity: f64) -> f64 {
        if self.peak_equity <= 0.0 {
            return 0.0;
        }
        (self.peak_equity - equity) / self.peak_equity * 100.0
    }

    pub fn is_halted(&self) -> bool {
        self.halted.is_some()
    }

    pub fn halt_reason(&self) -> Option<&str> {
        self.halted.as_deref()
    }

    /// Manuel resume (operatör kararı) — peak mevcut equity'ye çekilir
    pub fn resume(&mut self) {
        self.peak_equity = self.current_equity;
        self.halted = None;
    }

    /// SABİT pozisyon sayısı yerine bakiyeye göre dinamik üst sınır:
    /// `min(cfg_max, floor(equity / equity_per_position))`.
    /// $111 bakiyeyle 5 pozisyon açmak 5×$10'luk zorunlu minimuma takılırdı.
    pub fn max_positions(&self, equity: f64, cfg_max: usize) -> usize {
        let by_equity = (equity / self.cfg.equity_per_position_usd.max(1.0)).floor();
        (by_equity as usize).min(cfg_max)
    }

    fn total_exposure(positions: &HashMap<String, Position>) -> f64 {
        positions.values().map(|p| p.position_value.abs()).sum()
    }

    /// Emir öncesi kontrol. Ok: emir gönderilebilir.
    pub fn pre_trade(
        &self,
        equity: f64,
        positions: &HashMap<String, Position>,
        coin: &str,
        order_notional_usd: f64,
        leverage: f64,
        open_orders_for_coin: usize,
    ) -> Result<(), RiskViolation> {
        if let Some(r) = &self.halted {
            return Err(RiskViolation::Halted(r.clone()));
        }
        if equity < self.cfg.min_account_equity_usd {
            return Err(RiskViolation::EquityFloor(equity));
        }
        // MIN NOTIONAL: borsa $10 altını reddeder — emir hiç üretilmemeli
        if order_notional_usd < self.cfg.min_order_notional_usd {
            return Err(RiskViolation::TooSmall(order_notional_usd));
        }
        let dd = self.drawdown_pct(equity);
        if dd >= self.cfg.max_drawdown_pct {
            return Err(RiskViolation::Drawdown(dd));
        }
        let current = positions
            .get(coin)
            .map(|p| p.position_value.abs())
            .unwrap_or(0.0);
        let projected = current + order_notional_usd;
        if projected > self.cfg.max_position_notional_usd {
            return Err(RiskViolation::PositionLimit(projected, self.cfg.max_position_notional_usd));
        }
        let total = Self::total_exposure(positions) + order_notional_usd;
        if total > self.cfg.max_total_notional_usd {
            return Err(RiskViolation::TotalExposure(total, self.cfg.max_total_notional_usd));
        }
        if leverage > self.cfg.max_leverage {
            return Err(RiskViolation::Leverage(leverage, self.cfg.max_leverage));
        }
        if open_orders_for_coin + 1 > self.cfg.max_open_orders_per_coin {
            return Err(RiskViolation::OpenOrders(
                open_orders_for_coin + 1,
                self.cfg.max_open_orders_per_coin,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── MİN NOTİONAL + DİNAMİK POZİSYON SAYISI ─────────────────────────────
    #[test]
    fn min_notional_alti_emir_reddedilir() {
        let r = RiskManager::new(RiskConfig::default());
        let pos = HashMap::new();
        // $8 tahsis → borsa reddeder; emir ÜRETİLMEMELİ
        let e = r.pre_trade(1000.0, &pos, "BTC", 8.0, 1.0, 0);
        assert!(matches!(e, Err(RiskViolation::TooSmall(v)) if v == 8.0));
        // $10 üzeri geçer
        assert!(r.pre_trade(1000.0, &pos, "BTC", 25.0, 1.0, 0).is_ok());
    }

    #[test]
    fn pozisyon_sayisi_bakiyeye_gore_dinamik_kisalir() {
        let r = RiskManager::new(RiskConfig::default());
        // $111 bakiye: 111/15 = 7 → min(5, 7) = 5
        assert_eq!(r.max_positions(111.0, 5), 5);
        // $50 bakiye: 50/15 = 3 → 3 pozisyon (her biri en az ~$16)
        assert_eq!(r.max_positions(50.0, 5), 3);
        // $30 bakiye: 2 pozisyon
        assert_eq!(r.max_positions(30.0, 5), 2);
        // $15 bakiye: 1 pozisyon (sabit 5'in altına iner)
        assert_eq!(r.max_positions(15.0, 5), 1);
        // $10 bakiye: 0 → hiç pozisyon açılmaz ($10 min notional'ın altı)
        assert_eq!(r.max_positions(10.0, 5), 0);
    }

    #[test]
    fn dinamik_pozisyon_sayisi_asla_configi_gecmez() {
        let r = RiskManager::new(RiskConfig::default());
        assert_eq!(r.max_positions(1_000_000.0, 5), 5, "devasa bakiye bile 5'yi geçmemeli");
        assert_eq!(r.max_positions(0.0, 5), 0, "sıfır bakiyede pozisyon açılmaz");
    }
}
