//! STATE MACHINE — pozisyon yaşam döngüsü (idempotent).
//!
//! `Flat → Pending → InPosition → PendingExit → Flat`.
//! Garantiler:
//! 1. `on_decision`: Flat'ta eşik üstü skor → Place; Pending'te aynı
//!    yön → Nothing (çift emir yok); InPosition'da ters eşiği geçen skor →
//!    ExitNow (reduce_only, GERÇEK boyut).
//! 2. `force_exit` (B): ensemble exit kanalı → aynı garanti; Flat'ta
//!    HİÇBİR emir üretmez, PendingExit idempotent.
//! 3. `on_rejected`: red → Flat (yeniden giriş serbest).
//! 4. `on_order_status`: dolum/iptal geri beslemesi; bilinmeyen oid no-op.
//! 5. `oid_zero_pending`: yalnız oid==0 bekleyen girişte revert serbest.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn is_buy(self) -> bool {
        self == Side::Long
    }
}

/// Ensemble'tan gelen birleştirilmiş karar (strategy crate'ten ince kopya —
/// sirküler bağımlılık istenmez)
#[derive(Debug, Clone, Copy)]
pub struct Decision {
    /// +1 long, -1 short, sinyal gücü [-1,1]
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct BotOrder {
    pub coin: String,
    pub side: Side,
    pub sz: f64,
    /// limit fiyatı (defterden alınır)
    pub px: f64,
    /// post-only giriş (ALO) — MM için; market-like: Ioc
    pub post_only: bool,
    pub reduce_only: bool,
}

#[derive(Debug, Clone)]
pub enum Action {
    Place(BotOrder),
    Cancel(i64),
    /// kalanı market ile süpür (kısmi dolum timeout)
    Sweep(BotOrder),
    /// pozisyonu kapat (reduce_only)
    ExitNow(BotOrder),
    Nothing,
}

#[derive(Debug, Clone)]
pub struct SmConfig {
    /// borsadaki pozisyonun nilaişterilme eşiği (|skor| üstü ters sinyal → exit)
    pub reversal_min_score: f64,
    /// giriş için |skor| eşiği (ensemble min_score ile genelde aynı)
    pub entry_min_score: f64,
    /// trailing stop: zirveden bu ripe kadar geri çekilme -> exit (%)
    pub trailing_exit_pct: f64,
    /// kısmi dolum sonrası kalanın bekletme süresi (ms)
    pub pending_timeout_ms: u64,
    /// kullanılırsa: kaldıraç değil TODO boyutu (executor'da gerçek hesap)
    pub min_notional: f64,
}

impl Default for SmConfig {
    fn default() -> Self {
        Self {
            entry_min_score: 0.25,
            reversal_min_score: 0.4,
            trailing_exit_pct: 1.5,
            pending_timeout_ms: 15_000,
            min_notional: 10.0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PosState {
    Flat,
    Pending { side: Side, oid: i64, remaining: f64, ts: u64 },
    InPosition { side: Side, entry: f64, size: f64, peak: f64 },
    PendingExit { oid: i64, side: Side },
}

pub struct PositionStateMachine {
    pub coin: String,
    pub cfg: SmConfig,
    pub state: PosState,
}

impl PositionStateMachine {
    pub fn new(coin: &str, cfg: SmConfig) -> Self {
        Self { coin: coin.to_string(), cfg, state: PosState::Flat }
    }

    /// STATE RECOVERY: borsadaki gerçek pozisyondan başlat (restart senaryosu)
    pub fn recover(&mut self, existing: Option<(Side, f64 /*entry*/, f64 /*size*/, f64 /*mark*/)>) {
        match existing {
            Some((side, entry, size, mark)) if size.abs() > 1e-12 => {
                self.state = PosState::InPosition { side, entry, size, peak: mark };
            }
            _ => self.state = PosState::Flat,
        }
    }

    /// Ensemble kararı -> eylem. Öncek hem idempotent hem reversal-bilinçlidir.
    pub fn on_decision(&mut self, d: Decision, bid: f64, ask: f64, now_ms: u64) -> Action {
        match self.state.clone() {
            PosState::Flat => {
                if d.score >= self.cfg.entry_min_score {
                    self.state = PosState::Pending { side: Side::Long, oid: 0, remaining: 0.0, ts: now_ms };
                    Action::Place(BotOrder {
                        coin: self.coin.clone(), side: Side::Long, sz: 0.0, px: ask,
                        post_only: true, reduce_only: false,
                    })
                } else if d.score <= -self.cfg.entry_min_score {
                    self.state = PosState::Pending { side: Side::Short, oid: 0, remaining: 0.0, ts: now_ms };
                    Action::Place(BotOrder {
                        coin: self.coin.clone(), side: Side::Short, sz: 0.0, px: bid,
                        post_only: true, reduce_only: false,
                    })
                } else {
                    Action::Nothing
                }
            }
            PosState::Pending { side, oid, remaining: _, ts: _ } => {
                // idempotency: aynı yönde yine sinyal — yeni emir yok
                let same_dir = (side == Side::Long && d.score > 0.0)
                    || (side == Side::Short && d.score < 0.0);
                if same_dir {
                    // timeout kontrolü hariç: kalanın iptal + sweep (motor karar)
                    return Action::Nothing;
                }
                // ters taraf güçlü -> Pending'i iptal
                if d.score.abs() >= self.cfg.reversal_min_score {
                    if oid != 0 {
                        return Action::Cancel(oid);
                    }
                    return Action::Nothing;
                }
                Action::Nothing
            }
            PosState::InPosition { side, size, .. } => {
                // ters taraf güçlü -> kapat
                let against = (side == Side::Long && d.score <= -self.cfg.reversal_min_score)
                    || (side == Side::Short && d.score >= self.cfg.reversal_min_score);
                if against {
                    self.state = PosState::PendingExit { oid: 0, side };
                    Action::ExitNow(BotOrder {
                        // GERÇEK pozisyon boyutu — `sz: 0.0` placeholder borsaya
                        // 0-lot (hayalet) emir gönderiyordu.
                        coin: self.coin.clone(), side: side.flip(), sz: size,
                        px: if side == Side::Long { bid } else { ask },
                        post_only: false, reduce_only: true,
                    })
                } else {
                    Action::Nothing
                }
            }
            PosState::PendingExit { .. } => Action::Nothing,
        }
    }

    /// **B — SIGNAL::EXIT KANALI**: ensemble exit_requested → gerçek ExitNow.
    ///
    /// Yalnız `InPosition` emir üretir; Flat/Pending/PendingExit → `Nothing`
    /// (Flat pozisyonda Exit HİÇBİR emir üretmez; PendingExit → idempotent,
    /// ikinci ExitNow yok).
    pub fn force_exit(&mut self, bid: f64, ask: f64) -> Action {
        match self.state.clone() {
            PosState::InPosition { side, size, .. } => {
                self.state = PosState::PendingExit { oid: 0, side };
                Action::ExitNow(BotOrder {
                    coin: self.coin.clone(),
                    side: side.flip(),
                    sz: size,
                    px: if side == Side::Long { bid } else { ask },
                    post_only: false,
                    reduce_only: true,
                })
            }
            _ => Action::Nothing,
        }
    }

    /// Emir açıldıktan sonra engine, dönen oid'i makineye bağlar.
    pub fn link_pending_oid(&mut self, oid: i64) {
        if let PosState::Pending { oid: o, .. } | PosState::PendingExit { oid: o, .. } = &mut self.state {
            *o = oid;
        }
    }

    /// API reddi -> Flat (stratejiye "emir yok" bilgisi)
    pub fn on_rejected(&mut self, oid: i64) -> Action {
        match &self.state {
            PosState::Pending { oid: poid, .. } | PosState::PendingExit { oid: poid, .. }
                if *poid == oid =>
            {
                self.state = PosState::Flat;
                Action::Nothing
            }
            _ => Action::Nothing,
        }
    }

    /// Emir status olayı: (filled?, kalan, avgPx)
    pub fn on_order_status(&mut self, oid: i64, filled_size: f64, remaining: f64, avg_px: f64) -> Action {
        match self.state.clone() {
            PosState::Pending { side, remaining: _, .. } if oid == pending_oid(&self.state).unwrap_or(0) => {
                if remaining <= 1e-12 {
                    // tam dolum
                    self.state = PosState::InPosition { side, entry: avg_px, size: filled_size, peak: avg_px };
                    Action::Nothing
                } else {
                    // kısmi — kalan devam ediyor
                    if let PosState::Pending { remaining: r, .. } = &mut self.state {
                        *r = remaining;
                    }
                    Action::Nothing
                }
            }
            PosState::PendingExit { oid: poid, side } if oid != 0 && oid == poid => {
                if remaining <= 1e-12 {
                    self.state = PosState::Flat;
                }
                let _ = side;
                Action::Nothing
            }
            _ => Action::Nothing,
        }
    }

    /// Fiyat tikleri: trailing stop + timeout
    pub fn on_price_tick(&mut self, price: f64, now_ms: u64) -> Action {
        match self.state.clone() {
            PosState::InPosition { side, entry, size, peak } => {
                let new_peak = match side {
                    Side::Long => peak.max(price),
                    Side::Short => peak.min(price),
                };
                let draw = match side {
                    Side::Long => (new_peak - price) / new_peak.max(1e-12),
                    Side::Short => (price - new_peak) / new_peak.max(1e-12),
                };
                if draw * 100.0 >= self.cfg.trailing_exit_pct {
                    self.state = PosState::PendingExit { oid: 0, side };
                    return Action::ExitNow(BotOrder {
                        coin: self.coin.clone(), side: side.flip(),
                        sz: size, px: price, post_only: false, reduce_only: true,
                    });
                }
                if let PosState::InPosition { peak: p, .. } = &mut self.state {
                    *p = new_peak;
                }
                let _ = entry;
                Action::Nothing
            }
            PosState::Pending { oid, remaining, ts, .. } => {
                // kısmi dolum timeout: kalanı iptal et
                if now_ms - ts > self.cfg.pending_timeout_ms && remaining > 1e-12 && oid != 0 {
                    if let PosState::Pending { remaining: r, .. } = &mut self.state { *r = 0.0; }
                    Action::Cancel(oid)
                } else {
                    Action::Nothing
                }
            }
            _ => Action::Nothing,
        }
    }

    /// Pending, oid bekliyor (0-sız)? tahsis reddinde düz revert koşulu
    pub fn oid_zero_pending(&self) -> bool {
        matches!(self.state, PosState::Pending { oid: 0, .. })
    }

    pub fn in_position(&self) -> Option<(Side, f64, f64)> {
        match &self.state {
            PosState::InPosition { side, entry, size, .. } => Some((*side, *entry, *size)),
            _ => None,
        }
    }
}

fn pending_oid(s: &PosState) -> Option<i64> {
    match s {
        PosState::Pending { oid, .. } | PosState::PendingExit { oid, .. } => Some(*oid),
        _ => None,
    }
}

impl Side {
    pub fn flip(self) -> Side {
        match self { Side::Long => Side::Short, Side::Short => Side::Long }
    }
}

/// Tüm coinlerin makineleri (engine_loop kullanır)
pub type Machines = HashMap<String, PositionStateMachine>;

#[cfg(test)]
mod tests {
    use super::*;

    fn sm(coin: &str) -> PositionStateMachine {
        PositionStateMachine::new(coin, SmConfig::default())
    }

    #[test]
    fn servis_flat_to_pending_to_inposition() {
        let mut m = sm("BTC");
        let a = m.on_decision(Decision { score: 0.5 }, 99.0, 101.0, 1000);
        match &a {
            Action::Place(o) => {
                assert_eq!(o.side, Side::Long);
                assert_eq!(o.px, 101.0); // ask'e girer
            }
            _ => panic!("place bekleniyordu"),
        }
        // aynı sinyil tekrarı — IDEMPOTENT (2. emir yok)
        let a2 = m.on_decision(Decision { score: 0.6 }, 99.0, 101.0, 1100);
        assert!(matches!(a2, Action::Nothing));
        // oid geldi + emir doldu
        m.link_pending_oid(1);
        let a3 = m.on_order_status(1, 0.01, 0.0, 101.0);
        assert!(matches!(a3, Action::Nothing));
        let pos = m.in_position().unwrap();
        assert_eq!(pos.0, Side::Long);
        assert_eq!(pos.1, 101.0);
    }

    #[test]
    fn reddedilme_flata_doner() {
        let mut m = sm("BTC");
        let _ = m.on_decision(Decision { score: 0.5 }, 99.0, 101.0, 1000);
        m.on_order_status(2, 0.0, 0.01, 0.0); // oid yok
        // gerçek: rejected
        let a = m.on_rejected(0);
        assert!(matches!(a, Action::Nothing));
        assert!(matches!(m.state, PosState::Flat));
    }

    #[test]
    fn kismi_dolum_ve_timeout() {
        let mut m = sm("BTC");
        m.state = PosState::Pending { side: Side::Long, oid: 77, remaining: 0.01, ts: 1000 };
        // kısmi: 3/10 doldu, 7 kaldı
        let a = m.on_order_status(77, 0.003, 0.007, 100.0);
        assert!(matches!(a, Action::Nothing));
        assert!(matches!(m.state, PosState::Pending { oid: 77, remaining: r, .. } if (r - 0.007).abs() < 1e-12));
        // timeout: kalanı iptal
        let a = m.on_price_tick(100.0, 1000 + 16_000);
        assert!(matches!(a, Action::Cancel(77)));
    }

    #[test]
    fn trailing_stop_iklil() {
        let mut m = sm("BTC");
        m.state = PosState::InPosition { side: Side::Long, entry: 100.0, size: 0.01, peak: 100.0 };
        // zirve takibi: 110'da peak tazelendi
        let _ = m.on_price_tick(110.0, 1000);
        // 106'ya düşüş: (110-106)/110 = %3.6 > %1.5 -> EXIT
        let a = m.on_price_tick(106.0, 1100);
        match a {
            Action::ExitNow(o) => {
                assert_eq!(o.side, Side::Short); // kapatma ters yön
                assert!(o.reduce_only);
            }
            _ => panic!("exit bekleniyordu"),
        }
    }

    #[test]
    fn ters_sinyal_pozisyonu_kapar() {
        let mut m = sm("BTC");
        m.state = PosState::InPosition { side: Side::Long, entry: 100.0, size: 0.01, peak: 100.0 };
        let a = m.on_decision(Decision { score: -0.6 }, 99.0, 101.0, 1000);
        // GERİ DÖNÜŞ: kapatma emri GERÇEK pozisyon boyutunu taşımalı (0-lot
        // hayalet emir borsaya gidiyordu).
        match a {
            Action::ExitNow(o) => {
                assert_eq!(o.side, Side::Short);
                assert!(o.reduce_only);
                assert!((o.sz - 0.01).abs() < 1e-12, "çıkış boyutu 0: {}", o.sz);
            }
            _ => panic!("ExitNow bekleniyordu"),
        }
    }

    #[test]
    fn recovery_kurulumu() {
        let mut m = sm("BTC");
        // restart: borsada pozisyon VAR
        m.recover(Some((Side::Short, 200.0, 1.0, 198.0)));
        assert!(m.in_position().is_some());
        assert!(matches!(m.state, PosState::InPosition { side: Side::Short, .. }));
        // boş hesap
        let mut m2 = sm("ETH");
        m2.recover(None);
        assert!(matches!(m2.state, PosState::Flat));
    }
}
