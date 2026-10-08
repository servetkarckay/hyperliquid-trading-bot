# TEST MATRIX — Round 2 Kanıt Haritası

Durum: ✓=testle kanıtlı · kısmi · ✗=koşturulmadı/UNVERIFIED · R=runtime · S=source-scan · U=unit · I=integration · P=property · F=fuzz-fallback · E=E2E-canlı

| # | Bölüm | Durum | Kanıt/tip |
|---|---|---|---|
| 1 | Repo haritalama + bağımlılık grafiği | ✓ | workspace: api/data/engine/math/strategy/utils; START: bot.rs; döngü: run_cycle |
| 2 | FMT | ✓ U | `cargo fmt --all -- --check` temiz; 51 dosyaçe mekanik düzeltme |
| 2 | CHECK/TEST/BUILD --all-features | ✓ | tüm matris koşuldu: check ✓ · test 316/0 ✓ · release ✓ |
| 2 | CLIPPY | kısmi | mekanik 0; yapısal 41 (result_large_err 38 + args 3) — borç raporlu |
| 3 | Unit davranış | ✓ | 316 test: nominal + boundary + invalid NaN/lndev + failure + recovery |
| 4 | Data ingestion | ✓ | num()=Option; sahte-0 yasak; bozuk universe hacim loglanır |
| 5 | Storage failure | ✓ | exec retry→Err; satır-rejekt; 9 mock-HH testi |
| 6 | Data→Strateji kontrat | ✓ | strat_test_helpers; missing/stale/zero/saat fiyat → SKIP |
| 7 | Strateji gerçeklik | ✓ | 15 strateji skillet; kanonik isim + kurallar |
| 8 | Future leakage | ✓ | pivot/determinizm/interp kanıtları (Wyckoff/Elliott/Dow) |
| 9 | Ensemble/Regimc | ✓ | HOLD/Sklp/bloke kombinasyonları; min_score 0.30 |
| 10 | Risk stres | ✓ | exact boundary ± epsilon; peak/halt |
| 11 | Aynı-cycle cumulative | ✓ | CycleProjection: 40+30+30 kabul / 40+30+31 red; red → proje eklenmez; exit eklenmez |
| 12 | Leverage gate | ✓ | fail → entry absent; SL/TP bloklanmaz |
| 13 | Order state matrisi | ✓ kısmi | mock unit: accept/reject/unknown/500; canlı partial-fill/delayed: ✗ (UNVERIFIED) |
| 14 | UNKNOWN/benagzic | ✓ R | ReconciliationPending + bounded recheck; erken-Flat yasak; 3+9 test |
| 15 | CLOID idempotency | ✓ | tid dedup; restart çift-yazmama |
| 16 | Price precision | ✓ P | perp_price merkez; property test 10k × 5 szd; tick-sınır |
| 17 | Size/notional | ✓ U | min-notional/lot yuvarlama/0 lot hayırlar |
| 18 | State machine geçiş | ✓ | geçersiz geçiş guard; çift-event idempotent |
| 19 | SL/TP/trailing | ✓ | trailing gevşeme yasak; replace fail → koruma |
| 20 | Pozisyon hesap | ✓ | fill/avg/R; çift fill → çift PnL YOK |
| 21 | Learning/portfolio | ✓ | 1 trade=1 outcome; restart çift yükleme yok; unknown→0R yasak |
| 22 | Persistence/restart | ✓ | recovery testleri; machines/peak/halt |
| 23 | Concurrency senaryolar | kısmi | A-J paslar (WS+REST aynı fill vs.) mock-ile ✓; tam matris değil |
| 24 | Task/channel | ✓ R | keepalive helper 4 runtime test; spawn envanter; pump |
| 25 | Time/clock | kısmi | candle boundary ✓; injectable clock ✗ |
| 26 | Numerical stability | ✓ | NaN/Inf/div-zero guard setleri |
| 27 | Property-based | ✓ P | perp_price 10k; (proptest kütüphanesi yok — kendini xorshift + CI-deterministik) |
| 28 | Fuzz | kısmi F | libFuzzer yok; 20k randomized parser fallback; panic yok |
| 29 | Secret güvenlik | ✓ | .env tracked değil; kaynakta gerçek key yok |
| 30 | Logging | ✓ | cloid/fiyat/boyut lifecycle; secret yok |
| 31 | Failure injection | kısmı | mock: timeout/500/malformed; canli partial ✗ |
| 32 | E2E 20-senaryo | kısmi E | canlı testnet zinciri CANLI KANIT (7 order → fill → SL/TP → epoch 32/100); 20 senaryo matrisi tam değil |
| 33 | Ekonomik sınır | ✓ | rapor kâr iddiası İÇERMEZ; CANLI oluşum sayısı veri — kârlılık iddiası YOK |
| 34 | Coverage | ✗ | llvm-cov yok |
| 35 | CI pipeline | ✓ | .github/workflows/pr-ci.yml (fmt+clippy+test+build) + testnet-e2e.yml (manuel dispatch, mainnet guard) |
| 36-37 | Fail çözüm dürüstlük | ✓ | regression-first; test gevşetme yok |
| 38-39 | Makine-okunur rapor | ✓ | bu + AUDIT_REPORT + CI_AUDIT_SUMMARY |

## Spawn envanteri (§4)

| Görev | Sahip | Önem |
|---|---|---|
| bot.rs kullanıcı-akışı WS | epizotsal keepalive helper (4 runtime test) | öldü → log + 5sn respawn |
| ingestor shard ×2 | aynı helper | aynı |
| ingestor pump | WsEvent→StreamEvent forward; send-fail → return (abone kapandı = doğal son) | leak yok |
| executor: leverage jobs | bounded timeout 8s JoinHandle; collect | deadlock yok |
| executor: TP/SL arka-plan | tek-yapı cloid; sl_tx kanalı ile oid sahibe yazılır (drain_sl_results) | çift-stop yasak |
| storage mock spawn | test-only | — |
| utils NTP thread | join().ok()? — best-effort (kayanardı) | LOW |
