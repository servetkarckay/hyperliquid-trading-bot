//! ENSEMBLE — 18 stratejinin oylarını tek karara birleştirir.
//!
//! AMAÇ: stratejiler bağımsız oy verir (Long/Short/Hold/Skip); ensemble:
//!   1. Skip'leri EXCLUDE eder (sahiyle sayılmaz)
//!   2. Ağırlıklı ortalama skor hesaplar (ağırlıklar dizinden)
//!   3. Rejim filtresi: SQUEEZE/VOL_SPIKE rejimlerinde yeni giriş YASAK
//!   4. |skor| >= min_score -> karar; değilse nötr
//!
//! OVERFITTING KORUMASI: ağırlıklar sabit diziden gelir (canlıda gerçek
//! fill performansıyla güncellenir — dinamik karşılığı burada).
//! NOT: çelişen oylar birbirini götürür; net skor karar verir.

use std::collections::HashMap;

use crate::{Signal, Strategy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Long,
    Short,
    Neutral,
}

#[derive(Debug, Clone)]
pub struct ConsensusSignal {
    pub dir: Dir,
    /// ağırlıklı net skor [-1, 1]
    pub score: f64,
    /// (strateji adı, katkı, kısa not)
    pub contributors: Vec<(String, f64)>,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct EnsembleConfig {
    /// strateji adı -> ağırlık (verilmeyen: 1.0)
    pub weights: HashMap<String, f64>,
    /// karar eşiği (|skor| altı nötr)
    pub min_score: f64,
    /// güç komşu stratejilerin skorlarını sönümleyen faktör (config: <1 güven zayıf)
    pub damping: f64,
}

impl Default for EnsembleConfig {
    fn default() -> Self {
        // eşit ağırlık başlangıcı; canlıda backtest/win-rate ile güncellenir
        Self { weights: HashMap::new(), min_score: 0.25, damping: 1.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegimeGate {
    /// yeni girişe açık
    Open,
    /// giriş yasak (sıkışma — breakout bekleniyor; vol patlaması — küçült)
    Blocked,
    /// trend rejimi — momentum lehine çarpan
    TrendBoost,
}

pub struct Ensemble {
    pub cfg: EnsembleConfig,
}

impl Ensemble {
    pub fn new(cfg: EnsembleConfig) -> Self {
        Self { cfg }
    }

    /// Stratejilerin sinyallerini tek karara birleştirir.
    pub fn combine(&self, signals: Vec<(String, Signal)>) -> ConsensusSignal {
        let mut wsum = 0.0;
        let mut score = 0.0;
        let mut contributors = Vec::new();
        let mut notes = Vec::new();

        for (name, sig) in signals {
            let w = self.cfg.weights.get(&name).copied().unwrap_or(1.0);
            match sig {
                Signal::Long { strength, note } => {
                    score += w * strength;
                    wsum += w;
                    contributors.push((name.clone(), w * strength));
                    notes.push(format!("{name}[+{strength:.2}]: {note}"));
                }
                Signal::Short { strength, note } => {
                    score -= w * strength;
                    wsum += w;
                    contributors.push((name.clone(), -w * strength));
                    notes.push(format!("{name}[-{strength:.2}]: {note}"));
                }
                Signal::Exit { note } => {
                    // Exit oyu pozisyona muhtemel kapanış bilgisi; skora nötr etkiler
                    notes.push(format!("{name}[EXIT]: {note}"));
                }
                Signal::Hold { note } => {
                    notes.push(format!("{name}[HOLD]: {note}"));
                }
                Signal::Skip { reason } => {
                    notes.push(format!("{name}[SKIP]: {reason}"));
                }
            }
        }

        let mut final_score = if wsum > 0.0 { score / wsum } else { 0.0 };
        final_score *= self.cfg.damping;

        let dir = if final_score >= self.cfg.min_score {
            Dir::Long
        } else if final_score <= -self.cfg.min_score {
            Dir::Short
        } else {
            Dir::Neutral
        };

        ConsensusSignal {
            dir,
            score: final_score.clamp(-1.0, 1.0),
            contributors,
            note: if notes.len() > 3 {
                format!("{} (+{} bileşen)", notes[..3].join(" | "), notes.len() - 3)
            } else {
                notes.join(" | ")
            },
        }
    }

    /// Rejim filtresi: engelli rejimde Long/Short kararı nötr'e düşer.
    pub fn apply_regime(&self, cs: ConsensusSignal, gate: RegimeGate) -> ConsensusSignal {
        match gate {
            RegimeGate::Open => cs,
            RegimeGate::Blocked => {
                if cs.dir == Dir::Neutral {
                    cs
                } else {
                    ConsensusSignal { dir: Dir::Neutral, score: 0.0, contributors: cs.contributors, note: format!("REJİMENGELİ: {}", cs.note) }
                }
            }
            RegimeGate::TrendBoost => ConsensusSignal { score: (cs.score * 1.2).clamp(-1.0, 1.0), ..cs },
        }
    }
}

/// Registry üzerinden hepsini koşturan kompakt yardımcı
pub fn run_all_strategies(strategies: &[Box<dyn Strategy>], input: &crate::MarketInput) -> Vec<(String, Signal)> {
    strategies.iter().map(|s| (s.name().to_string(), s.evaluate(input))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sigs() -> Vec<(String, Signal)> {
        vec![
            ("A".into(), Signal::Long { strength: 0.6, note: "a".into() }),
            ("B".into(), Signal::Short { strength: 0.2, note: "b".into() }),
            ("C".into(), Signal::Hold { note: "c".into() }),
            ("D".into(), Signal::Skip { reason: "x".into() }),
        ]
    }

    #[test]
    fn oy_birlestirme() {
        let e = Ensemble::new(EnsembleConfig::default());
        let cs = e.combine(sigs());
        // A: +0.6, B: -0.2 (w=1) -> net 0.4/2 = 0.2 >= 0.25? hayır -> Neutral (0.2 < 0.25 minh)
        assert_eq!(cs.dir, Dir::Neutral);
        assert!((cs.score - 0.2).abs() < 1e-9);
        // Skip dışlandı (2 oyla hesap 3 değil)
    }

    #[test]
    fn agirlikli_oy() {
        let mut cfg = EnsembleConfig::default();
        cfg.weights.insert("A".into(), 3.0); // A baskın
        let e = Ensemble::new(cfg);
        let cs = e.combine(sigs());
        // (3*0.6 - 0.2) / 4 = 0.4 -> Long
        assert_eq!(cs.dir, Dir::Long);
        assert!((cs.score - 0.4).abs() < 1e-9);
    }

    #[test]
    fn rejim_ensusu() {
        let e = Ensemble::new(EnsembleConfig::default());
        let long_cs = e.combine(vec![("A".into(), Signal::Long { strength: 0.9, note: "x".into() })]);
        let blocked = e.apply_regime(long_cs.clone(), RegimeGate::Blocked);
        assert_eq!(blocked.dir, Dir::Neutral);
        let boosted = e.apply_regime(long_cs, RegimeGate::TrendBoost);
        assert!(boosted.score > 0.9);
    }

    #[test]
    fn negatif_konsensus() {
        let e = Ensemble::new(EnsembleConfig::default());
        let cs = e.combine(vec![
            ("A".into(), Signal::Short { strength: 0.7, note: "x".into() }),
            ("B".into(), Signal::Short { strength: 0.5, note: "y".into() }),
        ]);
        assert_eq!(cs.dir, Dir::Short);
    }
}

impl Ensemble {
    /// DİNAMİK KELLY YÜKLEME: ağırlıklar backtest/canlı-istatistikten.
    /// JSON biçimi: {"strat": {"sharpe": f, "win_rate": f, "profit_factor": f, "overfit": bool}, ...}
    /// overfit / sharpe<=0 + pf<1.2 / win_rate<%35 → ağırlık 0
    pub fn load_weights_json(&mut self, raw: &serde_json::Value) {
        let mut w: HashMap<String, f64> = HashMap::new();
        if let Some(map) = raw.as_object() {
            for (name, entry) in map {
                let overfit = entry.get("overfit").and_then(serde_json::Value::as_bool).unwrap_or(false);
                let sh = entry.get("sharpe").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                let wr = entry.get("win_rate").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                let pf = entry.get("profit_factor").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                // eğitimden gelen normalize ağırlık öncelikli; yoksa eski sezgisel
                let w_explicit = entry.get("weight").and_then(serde_json::Value::as_f64);
                let value = match w_explicit {
                    Some(x) if x > 0.0 => x,
                    Some(_) => 0.0,
                    None => {
                        if overfit || (sh <= 0.0 && pf < 1.2) || wr < 0.35 {
                            0.0
                        } else {
                            sh.max(0.05)
                        }
                    }
                };
                w.insert(name.clone(), value);
            }
        }
        self.cfg.weights = w;
    }
}
