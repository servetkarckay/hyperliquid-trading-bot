//! WYCKOFF — Accumulation/Distribution şematik analizi (profesyonel uyarlama).
//!
//! Uygulanan gerçek Wyckoff bileşenleri (referans: Wyckoff Method — üç yasa,
//! şematik olaylar, fazlar):
//!
//! **OLAYLAR**
//! - `SC`/`BC` klimaks: klimaks hacmi (>=2.5x 20-bar ort.) + pencere
//!   ekstremi + kapanışın bar ucundan belirgin uzaklığı (SC: düşüğün
//!   üstünden / BC: zirvenin altından). Kapanış-konumu şartsız klimaks
//!   YOK — paniğin içine anlık giriş yasak.
//! - `AR`/`AR-down`: klimakstan sonra otomatik tepki — aralığın öbür
//!   sınırı; anlamlı aralık ister (yükseklik >=1.2x ort. bar genişliği).
//! - `ST`/`LPSY` test: sınıra dönüş + AZALAN hacim (arz/talep emilimi
//!   kanıtı; hacmi artan test sinyali BASMAZ).
//! - `Spring`/`UT`: sınır ihlali + aralığa GERİ kapanış (sahte kırılım)
//!   — Wyckoff giriş fırsatı. İhlalin FİYATLA kalıcı olması aralığın
//!   bozulmasıdır: yapı iptal olur, giriş üretilmez.
//! - `Test`: spring'den sonra yüksek dip + azalan hacim -> faz C' —
//!   onaylı giriş (Wyckoff'un en merkezi kurulumu).
//! - `SOS`/`SOW`: geniş bar + yüksek hacimle aralık kırılışı -> faz D
//!   markup/markdown.
//!
//! **FAZLAR** (olay diziliminden çıkarım): A (SC/BC+AR) -> B (ST
//! toplama) -> C (Spring/UT — giriş fırsatı) -> C' (Test onaylı) -> D
//! (SOS/SOW) -> E (trend).
//!
//! **ÜÇ YASA**
//! - Effort/Result **divergence**: son 3 bar yüksek hacim ama dar sonuç
//!   (|c-o| <=0.25x ort. bar) — tanımın birebir hali; yönlü sinyali
//!   0.6x zayıflatır, eventsizken Hold'a uyarı eklenir.
//! - Cause/Effect: aralık yüksekliği (cause) girişlerin hedef notunu
//!   (effect) üretir: AR seviyesinden aralık-yüksekliği kadar projeksiyon.
//! - Arz/Talep: test hacim profiliyle değerlendirilir.
//!
//! **ÇIKIŞ** (`exit_intent`): klimaks hacmi + dar bar + 20-bar aralığın
//! ucu = effort-result çatışması — long'da dağıtım uyarısı (exit),
//! short'ta birikim uyarısı (exit). Emir kararı motorunkidir; niyet
//! stratejinindir.
//!
//! Deterministik pencere taraması (son N bar): aynı seri -> aynı sinyal;
//! gizli durum yok, restart/replay idempotent. Eksik veri -> Skip
//! (panik yok, sahte değer yok).
use crate::{closes, volumes, MarketInput, Signal, Strategy, ExitCtx, ExitIntent};

pub struct Wyckoff;

const MIN_BARS: usize = 120;
const WINDOW: usize = 40; // olay tarama penceresi
const AR_LOOKAHEAD: usize = 12; // klimakstan sonra AR arama
const CLIMAX_VOL_MULT: f64 = 2.5; // klimaks hacmi >= 2.5x ort.
const SOS_VOL_MULT: f64 = 1.3; // SOS/SOW hacmi
const CLIMAX_VOL_DECAY: f64 = 0.9; // test hacmi klimakstan azalan
const TEST_VOL_DECAY: f64 = 0.8; // test-onayı hacmi springden azalan
const CLOSE_OFF_PIVOT: f64 = 0.4; // klimaks kapanışı bar ucundan uzaklık
const RANGE_MIN_SPREADS: f64 = 1.2; // anlamlı aralık yüksekliği
const SPRING_BREAK_SPREADS: f64 = 0.1; // spring min ihlal derinliği
const BREAK_INVALID_SPREADS: f64 = 0.5; // fiyatla kapanış = yapı iptali
const NEAR_SUPPORT_SPREADS: f64 = 0.25; // test yakınlığı
const SOS_SPREAD_MULT: f64 = 1.3; // SOS bar genişliği
const DIVERGENCE_VOL_MULT: f64 = 1.6; // son-3-bar hacim eşiği
const DIVERGENCE_PROGRESS: f64 = 0.25; // dar sonuç eşiği (bar oranı)
const DIVERGENCE_DAMP: f64 = 0.6; // divergence sinyal zayıflatma

#[derive(Debug, Clone, Copy)]
struct Climax {
    tail_idx: usize,
    /// SC: bar düşüğü (arz duvarı altı) / BC: bar zirvesi
    level: f64,
    vol: f64,
}

#[derive(Debug, Clone, Copy)]
struct RangeInfo {
    climax: Climax,
    /// AR seviyesi: birikimde direnç, dağıtımda destek
    ar_level: f64,
    /// AR barının tail-idx (olaylar bundan SONRA aranır)
    ar_idx: usize,
}

#[derive(Debug, Default)]
struct AccumEvents {
    tests: u8,
    /// (tail_idx, spring low, spring vol)
    spring: Option<(usize, f64, f64)>,
    test_verified: bool,
    sos: bool,
}

#[derive(Debug, Default)]
struct DistriEvents {
    ut: Option<(usize, f64, f64)>, // (idx, ihlal tepesi, vol)
    lpsy: bool,
    sow: bool,
}

impl Wyckoff {
    /// Klimaks + AR çifti bul. `low_pivot=true` -> SC (birikim), değilse
    /// BC (dağıtım). Penceredeki EN YENİ geçerli çift döner.
    fn find_range(
        tail: &[crate::Candle],
        v_avg: f64,
        avg_spread: f64,
        low_pivot: bool,
    ) -> Option<RangeInfo> {
        if !(v_avg.is_finite() && v_avg > 0.0) || !(avg_spread.is_finite() && avg_spread > 0.0) {
            return None;
        }
        for i in (0..tail.len()).rev() {
            let c = &tail[i];
            let spread = (c.h - c.l).max(1e-12);
            let off = (if low_pivot { c.c - c.l } else { c.h - c.c }) / spread;
            if c.v < CLIMAX_VOL_MULT * v_avg || off < CLOSE_OFF_PIVOT {
                continue;
            }
            let extreme = if low_pivot { c.l } else { c.h };
            // klimaks ekstremi KENDİ ÖNCESİNE göredir: uzayan paniğin dibi
            // SC dipten aşağı inebilir (spring/ST) — SC kaybolmaz
            let before = &tail[..=i];
            let extreme_ok = if low_pivot {
                extreme <= before.iter().map(|x| x.l).fold(f64::MAX, f64::min) + 1e-12
            } else {
                extreme >= before.iter().map(|x| x.h).fold(f64::MIN, f64::max) - 1e-12
            };
            if !extreme_ok {
                continue;
            }
            // AR: klimakstan sonra en yüksek tepki (SC) / en alçak düzeltme (BC)
            let mut ar_lvl = if low_pivot { f64::MIN } else { f64::MAX };
            let mut ar_idx = usize::MAX;
            for j in (i + 1)..((i + 1 + AR_LOOKAHEAD).min(tail.len())) {
                let cand = if low_pivot { tail[j].h } else { tail[j].l };
                let better = if low_pivot { cand > ar_lvl } else { cand < ar_lvl };
                if better {
                    ar_lvl = cand;
                    ar_idx = j;
                }
            }
            if ar_idx == usize::MAX {
                continue;
            }
            let range_h = if low_pivot { ar_lvl - extreme } else { extreme - ar_lvl };
            if range_h < RANGE_MIN_SPREADS * avg_spread {
                continue;
            }
            return Some(RangeInfo {
                climax: Climax { tail_idx: i, level: extreme, vol: c.v },
                ar_level: ar_lvl,
                ar_idx,
            });
        }
        None
    }

    /// Birikim olaylarını AR'dan sonra tara (ST -> Spring -> Test -> SOS).
    fn scan_accumulation(
        tail: &[crate::Candle],
        r: &RangeInfo,
        v_avg: f64,
        avg_spread: f64,
    ) -> AccumEvents {
        let mut ev = AccumEvents::default();
        let support = r.climax.level;
        let resistance = r.ar_level;
        for j in (r.ar_idx + 1)..tail.len() {
            let b = &tail[j];
            // spring: sınır ihlali + FİYATLA geri kapanış (bir kez)
            if ev.spring.is_none() && b.l < support && b.c > support {
                ev.spring = Some((j, b.l, b.v));
                continue;
            }
            // fiyatla kapanmış aralık altı: test/spring DEĞİL — yapı riski
            if b.c < support {
                continue;
            }
            // SOS: hacim + geniş bar ile üst kırılış
            if b.c > resistance
                && (b.h - b.l) >= SOS_SPREAD_MULT * avg_spread
                && b.v >= SOS_VOL_MULT * v_avg
            {
                ev.sos = true;
                continue;
            }
            // test-onayı: spring'den sonra yüksek dip + azalan hacim + içte
            if let Some((_, s_low, s_vol)) = ev.spring {
                if !ev.test_verified
                    && b.l > support
                    && b.l > s_low
                    && b.v < s_vol * TEST_VOL_DECAY
                    && b.c >= support + NEAR_SUPPORT_SPREADS * avg_spread
                {
                    ev.test_verified = true;
                    continue;
                }
            }
            // ST: sınıra dönüş + klimakstan azalan hacim
            if b.l <= support + NEAR_SUPPORT_SPREADS * avg_spread
                && b.v < r.climax.vol * CLIMAX_VOL_DECAY
            {
                ev.tests = ev.tests.saturating_add(1);
            }
        }
        ev
    }

    /// Dağıtım olaylarını AR-down'dan sonra tara (LPSY -> UT -> SOW).
    fn scan_distribution(
        tail: &[crate::Candle],
        r: &RangeInfo,
        v_avg: f64,
        avg_spread: f64,
    ) -> DistriEvents {
        let mut ev = DistriEvents::default();
        let resistance = r.climax.level;
        let support = r.ar_level;
        for j in (r.ar_idx + 1)..tail.len() {
            let b = &tail[j];
            // UT: üst sınır ihlali + fiyatla içtarafa kapanış (bir kez)
            if ev.ut.is_none() && b.h > resistance && b.c < resistance {
                ev.ut = Some((j, b.h, b.v));
                continue;
            }
            // fiyatla kapanmış aralık üstü: LPSY/SOW DEĞİL
            if b.c > resistance {
                continue;
            }
            // SOW: hacim + geniş bar ile alt kırılış
            if b.c < support
                && (b.h - b.l) >= SOS_SPREAD_MULT * avg_spread
                && b.v >= SOS_VOL_MULT * v_avg
            {
                ev.sow = true;
                continue;
            }
            // LPSY: UT'den sonra daha alçak teppe + azalan hacim
            if let Some((_, u_top, u_vol)) = ev.ut {
                if !ev.lpsy && b.h < u_top && b.v < u_vol * TEST_VOL_DECAY {
                    ev.lpsy = true;
                    continue;
                }
            }
        }
        ev
    }

    /// Effort/Result divergence ölçümü: son 3 bar yüksek hacim + dar sonuç.
    fn divergence(tail: &[crate::Candle], vols: &[f64], v_avg: f64, avg_spread: f64) -> bool {
        let w = tail.len().min(3).max(1);
        let v3 = vols[vols.len() - w..].iter().sum::<f64>() / w as f64;
        let progress = tail[tail.len() - w..]
            .iter()
            .map(|b| (b.c - b.o).abs())
            .fold(0.0f64, f64::max);
        v3 >= DIVERGENCE_VOL_MULT * v_avg && progress <= DIVERGENCE_PROGRESS * avg_spread
    }

    /// Yönlü sinyale divergence basıncı uygular (0.6x güvence + not).
    fn damper(sig: Signal, divergent: bool) -> Signal {
        if !divergent {
            return sig;
        }
        match sig {
            Signal::Long { strength, note } => Signal::Long {
                strength: strength * DIVERGENCE_DAMP,
                note: format!("{note}; EFFORT/RESULT divergence (yüksek hacim dar sonuç) — güven 0.6x"),
            },
            Signal::Short { strength, note } => Signal::Short {
                strength: strength * DIVERGENCE_DAMP,
                note: format!("{note}; EFFORT/RESULT divergence — güven 0.6x"),
            },
            other => other,
        }
    }

    /// Eventsiz Hold'a divergence uyarısı ekler.
    fn hold_div(
        tail: &[crate::Candle],
        vols: &[f64],
        v_avg: f64,
        avg_spread: f64,
        msg: &str,
    ) -> Signal {
        let div = Self::divergence(tail, vols, v_avg, avg_spread);
        let note = if div {
            format!("{msg}; EFFORT/RESULT divergence uyarısı (yüksek hacim dar sonuç)")
        } else {
            msg.to_string()
        };
        Signal::Hold { note }
    }
}

impl Strategy for Wyckoff {
    fn name(&self) -> &'static str {
        "Wyckoff"
    }

    fn evaluate(&self, input: &MarketInput) -> Signal {
        let c = closes(&input.candles);
        if c.len() < MIN_BARS {
            return Signal::Skip {
                reason: format!("şematik tarama için >={MIN_BARS} mum gerekli"),
            };
        }
        let n = input.candles.len();
        let vol_head = volumes(&input.candles);
        let v_avg = vol_head[n - 20..].iter().sum::<f64>() / 20.0;
        let vol_tail = &vol_head[n - WINDOW..];
        let avg_spread =
            input.candles[n - 20..].iter().map(|x| x.h - x.l).sum::<f64>() / 20.0;
        if !(v_avg.is_finite() && v_avg > 0.0) || !(avg_spread.is_finite() && avg_spread > 0.0) {
            return Signal::Skip { reason: "hacim/spread hesaplanamaz".into() };
        }
        let tail = &input.candles[n - WINDOW..];
        let last = &input.candles[n - 1];

        // 1) iki şema taranır; en yeni geçerli dizilim yönü belirler
        let bull = Self::find_range(tail, v_avg, avg_spread, true); // SC+AR (birikim)
        let bear = Self::find_range(tail, v_avg, avg_spread, false); // BC+AR (dağıtım)
        if bull.is_none() && bear.is_none() {
            return Self::hold_div(
                tail,
                vol_tail,
                v_avg,
                avg_spread,
                "şematik olay yok (geçerli klimaks+AR bulunamadı)",
            );
        }
        let latest_of =
            |r: &Option<RangeInfo>| r.as_ref().map(|x| x.climax.tail_idx.max(x.ar_idx)).unwrap_or(0);
        let use_bull = match (&bull, &bear) {
            (Some(_), Some(_)) => latest_of(&bull) >= latest_of(&bear),
            (Some(_), None) => true,
            _ => false,
        };

        let divergent = Self::divergence(tail, vol_tail, v_avg, avg_spread);

        if use_bull {
            let r = bull.unwrap();
            let support = r.climax.level;
            let resistance = r.ar_level;
            // yapı bozulması: fiyatla kapanış, aralığın belirgin altında
            if last.c < support - BREAK_INVALID_SPREADS * avg_spread {
                return Signal::Hold {
                    note: format!(
                        "aralık [{support:.4};{resistance:.4}] fiyatla kırıldı — birikim İPTAL, yeni şematik bekleniyor"
                    ),
                };
            }
            let ev = Self::scan_accumulation(tail, &r, v_avg, avg_spread);
            if ev.sos {
                return Self::damper(
                    Signal::Long {
                        strength: 0.80,
                        note: format!(
                            "SOS: aralık [{support:.4};{resistance:.4}] hacim+geniş bar ile kırıldı — faz D markup; hedef ≈ {:.4}",
                            resistance + (resistance - support)
                        ),
                    },
                    divergent,
                );
            }
            if ev.test_verified {
                return Self::damper(
                    Signal::Long {
                        strength: 0.78,
                        note: format!(
                            "Test: spring doğrulandı (yüksek dip + azalan hacim), ST×{}; faz C'; hedef ≈ {:.4}",
                            ev.tests,
                            resistance + (resistance - support)
                        ),
                    },
                    divergent,
                );
            }
            if let Some((_, s_low, s_vol)) = ev.spring {
                let depth = (support - s_low).abs() / support.max(1e-12) * 100.0;
                // sakin spring (kaynak arz tükenmişliği) en güçlü; klimaks
                // hacimli spring (panik emilimi) ekstra onay
                let bonus = if s_vol <= 1.2 * v_avg {
                    0.10
                } else if s_vol >= CLIMAX_VOL_MULT * v_avg {
                    0.05
                } else {
                    0.0
                };
                let strength = (0.60 + 0.05 * ev.tests.min(2) as f64 + bonus).min(0.85);
                return Self::damper(
                    Signal::Long {
                        strength,
                        note: format!(
                            "Spring: destek {support:.4} sınırlı ihlali (yüzde {depth:.2}), aralığa geri kapanış; ST×{}; faz C",
                            ev.tests
                        ),
                    },
                    divergent,
                );
            }
            let phase = if ev.tests > 0 { "B" } else { "A" };
            return Self::hold_div(
                tail,
                vol_tail,
                v_avg,
                avg_spread,
                &format!(
                    "faz {phase}: birikim aralığı [{support:.4};{resistance:.4}]; giriş olayı bekleniyor (spring/test/SOS)"
                ),
            );
        }

        // 2) dağıtım dizilimi
        let r = bear.unwrap();
        let resistance = r.climax.level;
        let support = r.ar_level;
        if last.c > resistance + BREAK_INVALID_SPREADS * avg_spread {
            return Signal::Hold {
                note: format!(
                    "aralık [{support:.4};{resistance:.4}] fiyatla yukarı kırıldı — dağıtım İPTAL, yeni şematik bekleniyor"
                ),
            };
        }
        let ev = Self::scan_distribution(tail, &r, v_avg, avg_spread);
        if ev.sow {
            return Self::damper(
                Signal::Short {
                    strength: 0.80,
                    note: format!(
                        "SOW: aralık [{support:.4};{resistance:.4}] hacim+geniş bar ile kırıldı — faz D markdown; hedef ≈ {:.4}",
                        support - (resistance - support)
                    ),
                },
                divergent,
            );
        }
        if ev.lpsy {
            return Self::damper(
                Signal::Short {
                    strength: 0.78,
                    note: "LPSY: UT doğrulandı (daha alçak teppe + azalan hacim); faz D markdown".into(),
                },
                divergent,
            );
        }
        if let Some((_, top, _)) = ev.ut {
            return Self::damper(
                Signal::Short {
                    strength: 0.65,
                    note: format!("UT: direnç {top:.4} sahte ihlali + aralığa geri kapanış; faz C"),
                },
                divergent,
            );
        }
        Self::hold_div(
            tail,
            vol_tail,
            v_avg,
            avg_spread,
            &format!(
                "faz: dağıtım aralığı [{support:.4};{resistance:.4}]; olay bekleniyor (UT/LPSY/SOW)"
            ),
        )
    }

    /// ÇIKIŞ NİYETİ: klimaks hacmi + dar bar + 20-bar aralığın ucu =
    /// effort-result çatışması. Long'ta dağıtım uyarısı, short'ta
    /// birikim uyarısı. Emir kararı motorundadır; niyet buradadır.
    fn exit_intent(&self, ctx: &ExitCtx) -> Option<ExitIntent> {
        let pos = ctx.position.as_ref()?;
        let candles = &ctx.candles;
        if candles.len() < 20 {
            return None;
        }
        let vols = volumes(candles);
        let n = candles.len();
        let v_avg20 = vols[n - 20..].iter().sum::<f64>() / 20.0;
        let avg_spread20 =
            candles[n - 20..].iter().map(|x| x.h - x.l).sum::<f64>() / 20.0;
        if !(v_avg20.is_finite() && v_avg20 > 0.0) || !(avg_spread20.is_finite() && avg_spread20 > 0.0) {
            return None;
        }
        let last = &candles[n - 1];
        let range_h = candles[n - 20..].iter().map(|x| x.h).fold(f64::MIN, f64::max);
        let range_l = candles[n - 20..].iter().map(|x| x.l).fold(f64::MAX, f64::min);
        let span = (range_h - range_l).max(1e-12);
        let spread = (last.h - last.l).max(1e-12);
        let in_range = (last.c - range_l) / span;
        let narrow = spread <= 0.6 * avg_spread20;
        let climax = last.v >= 2.0 * v_avg20;
        if pos.is_long && climax && narrow && in_range >= 0.85 {
            return Some(ExitIntent {
                note: format!(
                    "dağıtım uyarısı: klimaks hacmi ({:.1}x ort) + dar bar + 20-bar teppe — effort yüksek, sonuç dar",
                    last.v / v_avg20.max(1e-12)
                ),
            });
        }
        if !pos.is_long && climax && narrow && in_range <= 0.15 {
            return Some(ExitIntent {
                note: format!(
                    "birikim uyarısı: klimaks hacmi ({:.1}x ort) + dar bar + 20-bar dip — effort yüksek, sonuç dar",
                    last.v / v_avg20.max(1e-12)
                ),
            });
        }
        None
    }
}

#[cfg(test)]
mod wyckoff_professional_tests {
    use super::*;
    use crate::strat_test_helpers::helpers as H;
    use crate::Candle;

    /// Anlık mum üretici: (o,h,l,c,v) dizisi → artan t'li seriler
    fn mk(start_idx: i64, rows: &[(f64, f64, f64, f64, f64)]) -> Vec<Candle> {
        rows.iter()
            .enumerate()
            .map(|(k, (o, h, l, c, v))| {
                H::candle(
                    (start_idx + k as i64) * 3_600_000,
                    *o, *h, *l, *c, *v,
                )
            })
            .collect()
    }

    /// Birikim dizilimi üretir (SC+AR) ve son barı taketover ile değiştirir
    fn accumulation_base() -> Vec<Candle> {
        let mut c = H::flat(92, 100.0); // tablo: v=100, spread 0.2
        c.extend(mk(92, &[
            // SC idx92 (tail idx 0): klimaks + düşük üstünden kapanış
            (100.0, 98.0, 95.0, 97.5, 400.0),
            // AR idx93 (tail idx 1): tepki → direnç 96.9
            (97.5, 96.9, 96.2, 96.5, 120.0),
            (96.5, 96.8, 95.8, 96.2, 90.0),
            // ST x2 (azalan hacim)
            (96.0, 96.1, 95.02, 95.9, 150.0),
            (96.0, 96.0, 95.01, 95.8, 140.0),
        ]));
        // idx97..: nötr bölge (aralık içi)
        let mut c2 = c.clone();
        c2.extend(H::flat(32, 96.4));
        c2
    }

    #[test]
    fn skip_yetersiz_mum() {
        let input = MarketInput { coin: "T".into(), candles: H::flat(100, 100.0), ..Default::default() };
        assert!(matches!(Wyckoff.evaluate(&input), Signal::Skip { .. }));
    }

    /// SPRING: destek ihlali + aralığa geri kapanış → Long (faz C).
    #[test]
    fn spring_long_uretir() {
        let mut c = accumulation_base();
        let n = c.len();
        // spring bar: 94.97 < 95 destek, c=95.5 > destek
        c.extend(mk(124, &[(95.6, 95.7, 94.97, 95.5, 180.0)]));
        c.extend(H::flat(n + 1 - 130, 96.4)); // c2 dengesi: toplamı 130 yap
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!(strength > 0.6 && strength <= 0.85, "spring gücü: {strength}");
                assert!(note.contains("Spring"), "{note}");
                assert!(note.contains("ST×2"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

    /// TEST: spring sonrası yüksek dip + azalan hacim → faz C' (0.78).
    #[test]
    fn test_spring_onayli_gucune_hucum() {
        let mut c = accumulation_base();
        c.extend(mk(124, &[(95.6, 95.7, 94.97, 95.5, 180.0)])); // spring
        c.extend(mk(125, &[(95.6, 95.8, 95.2, 96.1, 140.0)]));  // test-onay
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert_eq!(strength, 0.78);
                assert!(note.contains("Test"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

    /// SOS: aralığı hacim+geniş bar ile kırma → faz D markup.
    #[test]
    fn sos_aralik_kirilisi_long() {
        let mut c = accumulation_base();
        c.extend(mk(127, &[(96.9, 97.5, 96.9, 97.2, 260.0)]));
        c.extend(H::flat(2, 96.4));
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert_eq!(strength, 0.80);
                assert!(note.contains("SOS"), "{note}");
                // cause/effect hedef notu: direnç + aralık yüksekliği
                assert!(note.contains("hedef"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

    /// yapı BOZULMASI: fiyatla kapanış (spring değil) → giriş ÜRETİLMEZ.
    #[test]
    fn fiyati_kirilis_giris_uretmez() {
        let mut c = accumulation_base();
        c.extend(mk(129, &[(94.9, 95.0, 94.3, 94.4, 110.0)]));
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("İPTAL"), "{note}"),
            other => panic!("Hold beklenirdi: {other:?}"),
        }
    }

    fn distribution_base() -> Vec<Candle> {
        let mut c = H::flat(92, 100.0);
        c.extend(mk(92, &[
            // BC idx92 (tail 0): klimaks + zirvenin altından kapanış
            (100.0, 105.0, 101.0, 102.5, 420.0),
            // AR-down idx93 (tail 1): destek 101.4
            (102.5, 103.8, 101.4, 102.1, 110.0),
        ]));
        c.extend(H::flat(36, 104.0)); // idx94..129 (üst bölge)
        c
    }

    /// UT: üst sınır sahte ihlali + içtarafa kapanış → Short (faz C).
    #[test]
    fn ut_short_uretir() {
        let mut c = distribution_base();
        // idx127 flat alanında son barı UT yap
        let last_i = c.len() - 1;
        let (o, t) = (c[last_i].o, c[last_i].t);
        c[last_i] = H::candle(t, 104.5, 105.4, 104.2, 104.9, 220.0);
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Short { strength, note } => {
                assert_eq!(strength, 0.65);
                assert!(note.contains("UT"), "{note}");
            }
            other => panic!("Short beklenirdi: {other:?}"),
        }
    }

    /// LPSY: UT'den sonra alçak teppe + azalan hacim → faz D markdown (0.78).
    #[test]
    fn lpsy_dogrulanmis_short() {
        let mut c = distribution_base();
        let n = c.len();
        let (to, tt) = (c[n - 2].o, c[n - 2].t);
        c[n - 2] = H::candle(tt, 104.5, 105.4, 104.2, 104.9, 220.0); // UT
        let (lo, lt) = (c[n - 1].o, c[n - 1].t);
        c[n - 1] = H::candle(lt, 105.0, 105.2, 104.8, 105.0, 140.0); // LPSY
        let _ = (to, lo);
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Short { strength, note } => {
                assert_eq!(strength, 0.78);
                assert!(note.contains("LPSY"), "{note}");
            }
            other => panic!("Short beklenirdi: {other:?}"),
        }
    }

    /// SOW: aralığı hacim+geniş bar ile kırma → faz D markdown (0.80).
    #[test]
    fn sow_short() {
        let mut c = distribution_base();
        c.extend(mk(129, &[(101.6, 101.5, 100.6, 101.0, 260.0)]));
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Short { strength, note } => {
                assert_eq!(strength, 0.80);
                assert!(note.contains("SOW"), "{note}");
            }
            other => panic!("Short beklenirdi: {other:?}"),
        }
    }

    /// EFFORT/RESULT divergence: yüksek hacim + dar sonuç → Hold'a uyarı.
    #[test]
    fn divergence_uyarisi_ekler() {
        let mut c = H::flat(92, 100.0);
        c.extend(mk(92, &[
            (100.0, 98.0, 95.0, 97.5, 400.0),  // SC
            (97.5, 96.9, 96.2, 96.5, 120.0),   // AR → 96.9
            (96.5, 96.8, 95.8, 96.2, 90.0),
        ]));
        c.extend(H::flat(32, 96.4));
        c.pop(); c.pop(); c.pop();
        c.extend(mk(127, &[
            (96.0, 96.1, 95.98, 96.02, 210.0), // yüksek hacim, dar sonuç
            (96.0, 96.1, 95.98, 96.02, 210.0),
            (96.0, 96.1, 95.98, 96.02, 210.0),
        ]));
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Hold { note } => assert!(note.contains("divergence"), "{note}"),
            other => panic!("Hold beklenirdi: {other:?}"),
        }
    }

    /// divergence sinyal zayıflatır: spring 0.70 → 0.42 (0.6×).
    #[test]
    fn divergence_signal_0_6x() {
        let mut c = H::flat(92, 100.0);
        c.extend(mk(92, &[
            (100.0, 98.0, 95.0, 97.5, 400.0),
            (97.5, 96.9, 96.2, 96.5, 120.0),
            (96.5, 96.8, 95.8, 96.2, 90.0),
            (96.0, 96.1, 95.02, 95.9, 150.0),
            (96.0, 96.0, 95.01, 95.8, 140.0),
        ]));
        c.extend(H::flat(32, 96.4)); // pos97..128
        c.extend(mk(129, &[(95.6, 95.6, 94.97, 95.56, 500.0)])); // spring + dar sonuç
        let input = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        match Wyckoff.evaluate(&input) {
            Signal::Long { strength, note } => {
                assert!(strength <= 0.46, "0.6x zayıflamalı: {strength}");
                assert!(note.contains("divergence"), "{note}");
            }
            other => panic!("Long beklenirdi: {other:?}"),
        }
    }

    /// exit_intent (long): klimaks + dar bar + teppe → dağıtım uyarısı.
    #[test]
    fn exit_long_dagitim_uyarisi() {
        let mut c = H::flat(119, 100.0);
        c.extend(mk(119, &[(104.0, 104.05, 103.95, 104.0, 380.0)]));
        let ctx = ExitCtx {
            coin: "T".into(),
            candles: c.clone(),
            universe_returns: vec![],
            position: Some(crate::ExitPositionInfo { is_long: true, size: 1.0, entry_px: 100.0 }),
        };
        assert!(Wyckoff.exit_intent(&ctx).is_some(), "dağıtım uyarısı beklenirdi");
        // short aynı mumla → uyarı YOK (teppe bonusu)
        let ctx2 = ExitCtx {
            coin: "T".into(),
            candles: c.clone(),
            universe_returns: vec![],
            position: Some(crate::ExitPositionInfo { is_long: false, size: 1.0, entry_px: 100.0 }),
        };
        // teppe region'da short için "birikim uyarısı" koşulu at_bottom gerekmez
        assert!(Wyckoff.exit_intent(&ctx2).is_none(), "teppeda short exit olmamalı");
    }

    /// exit_intent (short): dip + klimaks + dar bar → birikim uyarısı.
    #[test]
    fn exit_short_birikim_uyarisi() {
        let mut c = H::flat(119, 100.0);
        c.extend(mk(119, &[(96.0, 96.05, 95.95, 96.0, 380.0)]));
        let ctx = ExitCtx {
            coin: "T".into(),
            candles: c,
            universe_returns: vec![],
            position: Some(crate::ExitPositionInfo { is_long: false, size: 1.0, entry_px: 100.0 }),
        };
        assert!(Wyckoff.exit_intent(&ctx).is_some());
    }

    /// IDEMPOTENT: aynı seriye tekrar çağrı → aynı sinyal (state yok).
    #[test]
    fn ayni_seri_ayni_sinyal() {
        let c = accumulation_base();
        let i1 = MarketInput { coin: "T".into(), candles: c.clone(), ..Default::default() };
        let i2 = MarketInput { coin: "T".into(), candles: c, ..Default::default() };
        assert_eq!(Wyckoff.evaluate(&i1), Wyckoff.evaluate(&i2));
    }
}
