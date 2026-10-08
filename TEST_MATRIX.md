# TEST MATRIX — Final Hardening

Durum: ✓=testle kanıtlı · kısmi · ✗=UNVERIFIED
Tip: R=runtime · S=source-scan · U=unit · I=integration · P=property · F=fuzz-fallback · E=E2E (mock) · T=canlı-testnet

| # | Bölüm | Durum | Kanıt/tip |
|---|---|---|---|
| 1 | Repo haritalama | ✓ | workspace: api/data/engine/math/strategy/utils; runtime: bot.rs; döngü: run_cycle |
| 2 | fmt | ✓ U | exit 0; 51 dosya mekanik |
| 2 | check/test/build all-features | ✓ | 343/0 · release 0 err |
| 2 | clippy -D warnings | ✓ | EXIT=0 — suppression YOK |
| 3 | Unit davranış | ✓ | 343 test: nominal+boundary+invalid+failure+recovery |
| 4 | Data ingestion | ✓ | num()=Option; sahte-0 yasak; bozuk universe log |
| 5 | Storage failure | ✓ U | exec retry→Err; satır red; 9 mock-HTTP |
| 6 | Data→Strategy kontrat | ✓ U | strat_test_helpers; missing/stale/zero→SKIP |
| 7 | Strateji gerçeklik | ✓ U | 15 strateji skillet (kanonik) |
| 8 | Future leakage | ✓ U | pivot/determinizm/interp |
| 9 | Ensemble/regime | ✓ U | HOLD/Skip/bloke; min_score |
| 10 | Risk stres | ✓ U | boundary ±ε; peak/halt |
| 11 | Aynı-cycle kümülatif | ✓ U | 40+30+30 kabul/101 red |
| 12 | Leverage gate | ✓ U/T | fail→entry yok; SL/TP bağımsız |
| 13 | Order state | kısmi U/T | accept/reject/unknown/500 mock ✓; canlı partial ✗ |
| 14 | UNKNOWN/recon | ✓ U | bounded recheck; erken Flat yasak |
| 15 | CLOID idempotency | ✓ U | tid dedup; restart çift-yazmama |
| 16 | Price precision | ✓ P | perp_price 10k×5 szd; tick-sınır |
| 17 | Size/notional | ✓ U | min-notional/lot/hayalet-koruma |
| 18 | State machine | ✓ U/M | geçiş matrisi; mutant-kanıt |
| 19 | SL/TP/trailing | ✓ U/M | gevşeme yasak; % mutant yakalandı |
| 20 | Pozisyon hesap | ✓ U | partial 25+25+50; PnL tek |
| 21 | Learning/portfolio | ✓ U | 1 trade=1 outcome; stats yetersiz-örnek=None |
| 22 | Persistence/restart | ✓ U | recover idempotent; canlı restart |
| 23 | Concurrency | kısmi R | keepalive 4 runtime test; lock/await 0; tam matris ✗ |
| 24 | Task/channel | ✓ R | keepalive+shard+pump; spawn envanter |
| 25 | Time/clock | kısmi | storm-pencere; injectable-clock ✗ (borç) |
| 26 | Numerical | ✓ U | NaN/Inf/div-zero |
| 27 | Property | ✓ P | perp_price 10k; xorshift deterministic |
| 28 | Fuzz | kısmi F | libFuzzer ✗; 20k randomized ✓ |
| 29 | Secret | ✓ | .env untracked; key yok |
| 30 | Logging | ✓ | lifecycle; secret yok |
| 31 | Failure injection | kısmi U | mock timeout/500; canlı ✗ |
| 32 | E2E matris | ✓ E/T | 20 senaryo (e2e_matrix.rs) + canlı zincir (7 order→fill→SL/TP→43/100) |
| 33 | Ekonomik sınır | ✓ | kâr iddiası YOK |
| 34 | Coverage | ✓ (ölçüldü) | 57.73% line · kritik modüller yüksek; executor/api borç |
| 35 | CI pipeline | ✓ | pr-ci.yml HARD + testnet-e2e.yml (manual+guard) |
| 36-37 | Dürüst çözüm | ✓ | regression-first; test gevşetme yok |
| 38-39 | Rapor | ✓ | AUDIT_REPORT + bu + CI_AUDIT_SUMMARY |
