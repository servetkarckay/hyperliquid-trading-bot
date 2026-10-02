# P0/P1/P2 paketi — backtest kaldırma, canlı cold-start eğitim, kalan düzeltmeler

## Özet

Eğitim tamamen canlıya taşındı. Backtest motoru ve onunla üretilmiş tüm ağırlıklar
silindi; 18 strateji **eşit ağırlıkla (1.0)** başlar, testnet'te gerçek
dolumlarla tracker üzerinden yavaşça öğrenir. "Neyin ne olduğu" artık gözle
görülür şekilde, biased veri olmadan oluşur.

| Commit | İş maddesi |
|---|---|
| `cba6841` | baseline (trailing stop + dolum-fiyatı stop düzeltmesi) |
| `5cf0cef` | **P0-A/B/C/D** — backtest kalksın; nötr cold-start + canlı öğrenme; veri-açlığı keşif kuyruğu; fee-farkındalıklı EV kapısı |
| (bu PR) | **P1-E/F/G + P2-H** — reduce-only tutarlılık assert'i, kill-switch taksonomisi + `ExitsOnly`, kesit momentum pencere doğrulaması, cloid ölçümü, doküman geçişi |

---

## Silinen satır sayısı (backtest)

| Dosya | Satır |
|---|---|
| `engine/src/backtest.rs` | 334 |
| `engine/src/backtest_multi.rs` | 196 |
| `engine/src/bin/backtest_train.rs` | 289 |
| **toplam** | **819** (+ 3 test) |

### Dokunma listesi ihlal edilmedi

| Korunan | Neden |
|---|---|
| `data/src/aggregator.rs` → `backfill()` | canlı göstergelerin ısınması için şart, backtest ile ilgisi yok |
| `data/src/backfill_job.rs` + ClickHouse arşivi + `collect.rs` | veri toplama altyapısı, offline analiz için kalıyor |
| `math/src/portfolio.rs` (Sharpe, min-variance, `kelly_binary`) | `strategy/src/risk_kelly.rs` kullanıyor |
| `storage.rs`'teki `hl_strategy_performance` tablosu | ileride canlı upsert hedefi |

Repo genelinde `backtest` geçen **kod referansı: 0**.

---

## Yeni ENV listesi

| ENV | Varsayılan | İş |
|---|---|---|
| `LEARN_MIN_N` | `10` | bu n altında ağırlık **donmuş 1.0** |
| `LEARN_FULL_N` | `30` | bu n üstünde tam kurallar + keşif kuyruğundan çıkış |
| `TRACK_STATE_PATH` | `config/track_state.json` | tracker persistence (atomik) |
| `ENSEMBLE_WEIGHTS_IN` | `config/ensemble_weights_live.json` | eski `ensemble_weights.json` **artık default değil** |
| `ENSEMBLE_LIVE_WEIGHTS_OUT` | `config/ensemble_weights_live.json` | aynı dosya (in=out) |
| `EV_TAKER_FEE_BPS` | `4.5` | round-trip maliyet girdisi |
| `EV_SLIPPAGE_BPS` | `5.0` | round-trip maliyet girdisi |
| `EV_MIN_DIST_BPS` | `75` | taban; `max(75, 3×round_trip, spread×3)` |
| `ENGINE_CRIT_THRESHOLD` | `5` | kill-switch eşiği (yalnız **karar-kritik** hatalar) |

---

## `[LEARN]` tablosundan bir örnek

Açılış (tek satır):
```
[LEARN] nötr başlangıç: 18 strateji × w=1.0; persistence=ON (min_n=10 full_n=30)
```

60 saniyede bir:
```
[LEARN] 18 strateji | kuyruk=18 | cursor=7
Dow Teorisi             w=1.00 n=0    win=-    pf=-    ΣR=+0.00
Kesit Momentum          w=1.00 n=8    win=38%  pf=0.94 ΣR=-0.40
Elliott Dalga           w=0.42 n=12   win=42%  pf=1.10 ΣR=-0.80
Optimal Execution       w=0.00 n=34   win=29%  pf=0.71 ΣR=-4.20
```
Ağırlık değişince ayrıca: `[LEARN] SUSTUR: Optimal Execution 1.00→0.00 (n=34 wr=0.29 pf=0.71)`

Keşif kuyruğu:
```
[KEŞİF] sıra: Kesit Momentum (n=8 < full_n=30) | kuyruk=18 | sinyal veren=4 | sıradakiler=["Kesit Momentum","Wyckoff",…]
```

## `[EV]` skip logundan bir örnek

```
[EV] BIGTIME ATR 167bps → risk 250bps < maliyet flooru 750bps → SKIP
     (round-trip 190bps; işlem sayısının düşmesi BEKLENEN davranış)
```

---

## Doğrulama

- `cargo build --release --workspace` → temiz
- `cargo test --release --workspace` → **engine 69**, strategy 4, hepsi yeşil
  (36 taban + 4 senaryo + 2 cloid + 2 stop-px + 4 sl-side + 8 trailing +
  14 cold-start/keşif/persistence + 6 EV + 8 health)

### Canlı testnet ölçümleri

| Ölçüm | Sonuç |
|---|---|
| **P2-H** `place → cancel → AYNI cloid → tekrar place` | **KABUL** (yeni oid). Cloid `tid` gibi dedup anahtarı **değil** → proses-içi monotonik sayaç **eklenmedi** (eklemek idempotency amacını bozardı). `signing::post` aynı gövdeyi retry eder → retry asla farklı cloid üretmez. |
| **P1-G** kesit momentum penceresi | Kod gerçeği **1 gün** (`mark_px/prev_day_px − 1`). 7g hesabı **yok** — doküman düzeltildi, `[AL]` logları da `1g=` diyor. |
| **P1-E** `openOrders` stop tutarlılığı | REST `triggerPx` vermiyor → `limitPx/1.05` (buy stop), `/0.95` (sell stop), tick'e yuvarlanır. `sl_last_px` ile karşılaştırılır; **>%0.1 sapma → yabancı emir**, `sl_orders`'a sayılmaz + `[SL-WARN]` |

---

## Kâr iddiası

**Yok.** Doğruluk kanıtlandı, kârlılık **kanıtlanmadı**. Testnet dolum
kalitesi gerçek likiditeyi temsil etmez; istatistikler **yön göstergesidir**.
*Canlı eğitim tamamlanana kadar kâr iddiası yoktur.*

Mainnet'e geçişte `track_state.json`'in taşınıp taşınmayacağı **kullanıcı
kararıdır** — bu pakette uygulanmadı.