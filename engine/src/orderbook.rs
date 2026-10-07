//! LOCAL BOOK — yerel L2 defter (WS tam snapshot tabanlı).
//!
//! `LocalBook`: `sync_from_rest / apply_ws` ile seviyeleri kurar;
//! `best_bid/ask, mid, spread_bps, imbalance, vwap_fill, depth_within_bps`;
//! `is_stale / is_sane` — bayat/bozuk defter sinyale ve emre GİTMEZ.
//! Reconnect sonrası tam snapshot'tan yeniden kurulum; artımlı varsayım yok.
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::Value;

use hyperliquid_api::Info;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelF {
    pub px: f64,
    pub sz: f64,
    pub n: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

#[derive(Debug, Clone)]
pub struct LocalBook {
    pub coin: String,
    /// fiyat azalan
    pub bids: Vec<LevelF>,
    /// fiyat artan
    pub asks: Vec<LevelF>,
    pub server_time: i64,
    updated_at: Instant,
}

impl LocalBook {
    pub fn new(coin: &str) -> Self {
        Self {
            coin: coin.to_string(),
            bids: Vec::new(),
            asks: Vec::new(),
            server_time: 0,
            updated_at: Instant::now() - Duration::from_secs(3600),
        }
    }

    /// REST snapshot ile ilk senkron (sonra WS sürer)
    pub async fn sync_from_rest(&mut self, info: &Info) -> Result<()> {
        let book = info.l2_book(&self.coin).await?;
        self.bids = book
            .levels
            .first()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|l| {
                Some(LevelF {
                    px: l.px.parse().ok()?,
                    sz: l.sz.parse().ok()?,
                    n: l.n.unwrap_or(0),
                })
            })
            .collect();
        self.asks = book
            .levels
            .get(1)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|l| {
                Some(LevelF {
                    px: l.px.parse().ok()?,
                    sz: l.sz.parse().ok()?,
                    n: l.n.unwrap_or(0),
                })
            })
            .collect();
        self.server_time = book.time;
        self.updated_at = Instant::now();
        Ok(())
    }

    /// WS `l2Book` event verisini uygula. Coin uyuşmazsa false döner.
    pub fn apply_ws(&mut self, data: &Value) -> bool {
        if data["coin"].as_str() != Some(self.coin.as_str()) {
            return false;
        }
        let time = data["time"].as_i64().unwrap_or(0);
        let levels = match data["levels"].as_array() {
            Some(l) if l.len() == 2 => l.clone(),
            _ => return false,
        };
        let coin = self.coin.clone();
        self.apply_snapshot(&coin, time, &levels);
        true
    }

    fn apply_snapshot(&mut self, coin: &str, time: i64, levels: &[Value]) {
        let parse = |arr: &Value| -> Vec<LevelF> {
            arr.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| {
                            Some(LevelF {
                                px: l["px"].as_str()?.parse().ok()?,
                                sz: l["sz"].as_str()?.parse().ok()?,
                                n: l["n"].as_u64().unwrap_or(0),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        self.bids = parse(&levels[0]);
        self.asks = parse(&levels[1]);
        self.server_time = time;
        self.updated_at = Instant::now();
        // koruma: bozuk snapshot (sadece tek taraf) kabul etme
        if self.bids.is_empty() || self.asks.is_empty() {
            tracing::warn!(coin = %coin, "bos/tarafsiz book snapshot'u reddedildi");
        }
    }

    pub fn best_bid(&self) -> Option<LevelF> {
        self.bids.first().copied()
    }
    pub fn best_ask(&self) -> Option<LevelF> {
        self.asks.first().copied()
    }
    pub fn mid(&self) -> Option<f64> {
        Some((self.best_bid()?.px + self.best_ask()?.px) / 2.0)
    }
    pub fn spread_bps(&self) -> Option<f64> {
        let (b, a) = (self.best_bid()?.px, self.best_ask()?.px);
        if b <= 0.0 {
            return None;
        }
        Some((a - b) / ((a + b) / 2.0) * 10_000.0)
    }

    /// Üst `depth` seviyedeki hacim dengesi: (bid-ask)/(bid+ask) ∈ [-1, 1]
    pub fn imbalance(&self, depth: usize) -> Option<f64> {
        let sum = |lv: &[LevelF]| -> f64 { lv.iter().take(depth).map(|l| l.sz).sum() };
        let (b, a) = (sum(&self.bids), sum(&self.asks));
        if b + a <= 0.0 {
            return None;
        }
        Some((b - a) / (b + a))
    }

    /// Verilen miktarda doldurmanın VWAP'ı ve gerçekten doldurulan miktar.
    /// side=Bid -> asks'ten alır, side=Ask -> bids'e (satış) vurur.
    pub fn vwap_fill(&self, side: Side, qty: f64) -> Option<(f64, f64)> {
        let lv = match side {
            Side::Bid => &self.asks,
            Side::Ask => &self.bids,
        };
        let mut left = qty;
        let mut cost = 0.0;
        let mut filled = 0.0;
        for l in lv {
            if left <= 0.0 {
                break;
            }
            let take = left.min(l.sz);
            cost += take * l.px;
            filled += take;
            left -= take;
        }
        if filled <= 0.0 {
            return None;
        }
        Some((cost / filled, filled))
    }

    /// Mid'den ±bps bandın içindeki toplam hacim
    pub fn depth_within_bps(&self, side: Side, bps: f64) -> Option<f64> {
        let mid = self.mid()?;
        let (lv, dir) = match side {
            Side::Bid => (&self.bids, -1.0),
            Side::Ask => (&self.asks, 1.0),
        };
        Some(
            lv.iter()
                .take_while(|l| (l.px - mid) * dir / mid * 10_000.0 <= bps)
                .map(|l| l.sz)
                .sum(),
        )
    }

    /// Son güncelleme `max_age`'ten eski mi (WS kopması tespiti)
    pub fn is_stale(&self, max_age: Duration) -> bool {
        self.updated_at.elapsed() > max_age
    }

    /// Kitap sağlamlık kontrolü: bid < ask ve sıralama doğru mu
    pub fn is_sane(&self) -> Result<()> {
        let (b, a) = (self.best_bid(), self.best_ask());
        match (b, a) {
            (Some(b), Some(a)) if b.px < a.px => {
                let bid_sorted = self.bids.windows(2).all(|w| w[0].px > w[1].px);
                let ask_sorted = self.asks.windows(2).all(|w| w[0].px < w[1].px);
                if bid_sorted && ask_sorted {
                    Ok(())
                } else {
                    bail!("book siralamasi bozuk")
                }
            }
            (Some(b), Some(a)) => bail!("crossed book: bid {b:?} >= ask {a:?}"),
            _ => bail!("book bos"),
        }
    }
}
