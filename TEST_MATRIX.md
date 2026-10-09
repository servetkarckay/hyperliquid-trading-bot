# TEST MATRIX — FINAL FREEZE

Durum: ✓ kanıtlı · kısmi · ✗ UNVERIFIED — Tip: R runtime · S source-scan · U unit · P property · F fuzz · E mock-E2E · T canlı-testnet

| # | Bölüm | Durum | Tip | Kanıt |
|---|---|---|---|---|
| 1 | Haritalama | ✓ | — | 6 crate; bot.rs; run_cycle |
| 2 | fmt/check/test/build/release/clippy | ✓ | U | tam matris: hepsi PASS (343/0; clippy EXIT=0) |
| 3 | Unit davranış | ✓ | U | 343: nominal+boundary+invalid+failure+recovery |
| 4 | Data ingestion | ✓ | U | num() sözleşme + fuzz |
| 5 | Storage failure | ✓ | U | exec retry→Err; 9 mock |
| 6 | Data→Strategy | ✓ | U | helpers; SKIP sözleşmesi |
| 7 | Strateji gerçeklik | ✓ | U | 15 kanonik skillet |
| 8 | Future leakage | ✓ | U | determinizm/interp |
| 9 | Ensemble/regime | ✓ | U | kombinasyonlar |
| 10 | Risk stres | ✓ | U | boundary; peak/halt |
| 11 | Cycle-kümülatif | ✓ | U | 101-red |
| 12 | Leverage gate | ✓ | U/T | fail→entry yok |
| 13 | Order state | ✓ kısmi | U/T | mock matris; canlı partial ✗ |
| 14 | UNKNOWN/recon | ✓ | U | bounded; erken-Flat yok |
| 15 | CLOID idempotency | ✓ | U | dedup; replay |
| 16 | Precision | ✓ | P/F | 10k + libFuzzer 5.26M |
| 17 | Size/notional | ✓ | U | min/lot/hayalet |
| 18 | State machine | ✓ | U/M | missed=0 (mutasyon) |
| 19 | SL/TP/trailing | ✓ | U/M | gevşeme yasak; % mutant yakalandı |
| 20 | Hesap | ✓ | U | partial avg; PnL tek |
| 21 | Learning/portfolio | ✓ | U | 1:1; yetersiz-örnek=None |
| 22 | Restart/recovery | ✓ | U/T | recover idempotent; canlı |
| 23 | Concurrency | kısmi | R | keepalive 4 test; lock 0 |
| 24 | Task/channel | ✓ | R/S | envanter; supervisor |
| 25 | Time | kısmi | — | injectable-clock ✗ (borç) |
| 26 | Numerical | ✓ | U | NaN/Inf |
| 27 | Property | ✓ | P | perp_price 10k |
| 28 | Fuzz | ✓ | F | libFuzzer 5.26M/0 crash + 20k fallback |
| 29 | Secret | ✓ | — | untracked; key yok |
| 30 | Logging | ✓ | — | lifecycle; secret yok |
| 31 | Failure injection | kısmı | U | mock timeout/500; canlı ✗ |
| 32 | E2E 20 | ✓ | E | 20 senaryo + canlı |
| 33 | Ekonomik | ✓ | — | iddia yok |
| 34 | Coverage | ✓ | — | 57.73 ölçüldü (borç: executor/api) |
| 35 | CI | ✓ | — | pr-ci HARD + testnet-e2e |
| 36-39 | Dürüstlük+rapor | ✓ | — | regression-first; bu set |

## MUTASYON MISSED KATEGORİZASYONU (947)

- transport/sync (run_cycle/ingest/sync — ağ-temaslı): 568 → DOCUMENTED DEBT
- binary main (bot startup env/arg): 140 → debt (entegrasyon-dışı)
- log/serialization (fmt_signals/line/report): ~120 → debt
- probe-arac (probe_px/smoke): 54 → araç-binary, debt
- allocation-math (conviction vb.): 58 → post-freeze test borcu
- ε-sınır (1e-12 ölçü-ölçüsüz): 11 → equivalent-in-practice

Kritik modüller (risk_manager/state_machine/track): missed = 0 ✓
