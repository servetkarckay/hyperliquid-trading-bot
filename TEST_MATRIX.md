# TEST MATRIX — Kanıt Haritası

Durum: ✓=testle kanıtlı, ✗=çalıştırılmadı/kanıtlanamadı (UNVERIFIED)

| # | Audit bölümü | Durum | Kanıt / Not |
|---|---|---|---|
| 1 | Repo haritalama | ✓ | workspace: api, data, engine, math, strategy, utils; runtime: bot.rs; docs/SISTEM-MIMARISI.md |
| 2 | fmt/check/test/build/clippy | kısmi | test+build ✓ (305/0; release 0 err); fmt+clippy ✗ (toolchain'de kurulu değil) |
| 3 | Unit davranış denetimi | ✓ | 305 test: nominal/boundary/invalid/failure/recovery setleri (aşağıda) |
| 4 | Data ingestion | ✓ | num()=Option sözleşmesi (29 data testi); bozuk universe hacim → görünür düşme (yeni) |
| 5 | Storage failure | ✓ | exec retry→Err; satır red; 9 mock-HTTP testi |
| 6 | Data→Strategy contract | ✓ | strat_test_helpers tek kaynak jeneratörler; missing/stale/zero/sabit fiyat testleri strateji suite'lerinde |
| 7 | Strateji gerçeklik denetimi | ✓ | 15 strateji kanonik denetimi (önceki oturumlar; her strateji kendi suite'i ile) |
| 8 | Future leakage | ✓ | pivot-öncesi-kapanış ve pencere-determinizm testleri (Wyckoff/Elliott/Dow determinism+interp kanıtları) |
| 9 | Ensemble/voting/regime | ✓ | all-Hold/mixed/Skip-yoğun/Exit-korunumu/regime-blocked testleri; min_score=0.30 |
| 10 | Risk stres | ✓ | exact-boundary/epsilon testleri; peak equity; halt; CycleProjection 6 test |
| 11 | Same-cycle cumulative risk | ✓ | 40+30+30=100 kabul / +31 red; red→projeye eklenmez; exit→eklemez |
| 12 | Leverage gate | ✓ | leverage fail → entry gönderilmez; SL/TP/flatten bloklanmaz |
| 13 | Order state matrix | kısmi | accept/reject/unknown/500/malformed mock testleri ✓; partial-fill/duplicate-response canlı-davranışı ✗ UNVERIFIED |
| 14 | UNKNOWN/Reconciliation | ✓ | ReconciliationPending evidence loop; erken-Flat yasağı; duplicate-entry gate; A-F senaryoları mock-ile ✓ |
| 15 | CLOID idempotency | ✓ | tid dedup; restart çift-yükleme yasağı (replace_all); WS+REST aynı event tek yazım |
| 16 | Price precision | ✓ | perp_price merkez; modify dahil; exec/datanın TÜM giden emirleri; szd 0..6 varyantları |
| 17 | Size/notional/min order | kısmi | min-notional boundary testi ✓; precision-truncation sonrası min altına düşme senaryosu ayrı testi ✗ (UNVERIFIED) |
| 18 | State machine geçiş matrisi | ✓ | geçersiz geçiş guard testleri; çift-event idempotent |
| 19 | SL/TP/trailing | ✓ | trailing gevşeme yasağı; replace fail → eski stop kalır testleri |
| 20 | Position accounting | ✓ | fill/avg-price/R; çift fill → çift PnL YOK |
| 21 | Learning/portfolio bütünlüğü | ✓ | 1 trade=1 portfolio outcome; attribution ayrı; unknown→0R yasak |
| 22 | Persistence/restart | ✓ | restart-recovery testleri; peak/halt korunumu; pozisyon state restart senaryoları |
| 23 | Concurrency | kısmi | deterministik testler var; controlled-barrier reproductions tam matris ✗ |
| 24 | Task/channel denetimi | ✓ | Bu oturum: WS+shard supervisor; send-failure/dropped-rx kalan noktalar gözden geçirildi (kalan: refresh best-effort'lar LOW) |
| 25 | Time/clock | kısmi | candle boundary + epoch testleri ✓; injectable-clock ✗ (wall-clock doğrudan) |
| 26 | Numerical stability | ✓ | div-zero/NaN guard strateji+math testlerinde; GARCH stationarity guard |
| 27 | Property-based | ✗ | proptest/quickcheck yok; invariant'lar örnek-bazlı testlerle |
| 28 | Fuzz | ✗ | kurulu hedef yok; parser örnek testleri var |
| 29 | Secret güvenliği | ✓ | .env tracked değil; kaynakta gerçek key yok; test dummy hariç |
| 30 | Logging/observability | ✓ | coin+cloid+side+qty+price+lifecycle logları; UNKNOWN gerekçe logu; secret loglanmıyor |
| 31 | Failure injection | kısmi | mock-HTTP: timeout/500/malformed/duplicate ✓; connection-reset/delayed/429 sentetik ✗ |
| 32 | End-to-end lifecycle | kısmi | altın-yol testi (entry→fill→SL→exit→track) ✓; 20-senaryo tam E2E matrisi ✗ |
| 33 | Ekonomik sınır | ✓ | raporlar kârlılık iddiası İÇERMEZ; UNVERIFIED işaretli |
| 34 | Coverage | ✗ | llvm-cov/mutation koşturulmadı |
| 35 | CI pipeline | ✗ | GitHub Actions workflow yok (öneri: fmt-install + clippy + test + release build + source-scan) |
| 36-37 | Fail çözüm dürüstlüğü | ✓ | B1-B4 önce regression-test sonra fix; gevşetme/ignore KULLANILMADI |
| 38 | Makine-okunur rapor | ✓ | AUDIT_REPORT.md + bu dosya + CI_AUDIT_SUMMARY.md |
