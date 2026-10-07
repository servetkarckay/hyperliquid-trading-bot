# System Audit — Hyperliquid Trading Bot

- commit: `48ca304` (Cancel result yutulamaz)
- toolchain: stable-x86_64-unknown-linux-gnu (rustfmt/clippy toolchain'e kurulu DEĞİL)
- test environment: cargo test --workspace (unit + source-scan + mock-HTTP); canlı testnet hesabı üzerinden canlı order gönderen test YOK
- date: 2026-10-07

## PASS (gerçekten doğrulanmış)

| Alan | Kanıt |
|---|---|
| Workspace test tabanı | `cargo test --workspace` → **305 passed / 0 failed** |
| Release build | `cargo build --release --workspace` → 0 error |
| WS task ölümü görünür + respawn | bot.rs/ingestor.rs supervisor (`abbe957`) + 3 source-scan test |
| Cancel sonucu yutulamaz | executor cancel_tp/cancel_sl → Ok/Err log ayrımı (`48ca304`) + 1 source-scan test |
| Evren hacim verisi bozuk → sessiz coin düşme YOK | ingestor `[UNIVERSE]` warn logu (`abbe957`); fail-closed korunur ama görünür |
| Storage yazma hatası sahte başarı üretmez | storage.rs exec retry→Err, satır-rejeksiyon sözleşmeleri, 9 mock-HTTP test (önceki oturum `40050e6`) |
| Sayı sözleşmesi: missing/invalid → None (sahte 0.0 YOK) | ingestor `num()` → Option; 29 data testi (dahil `num_sozlesme_testleri`) |
| UNKNOWN → anında Flat YASAK | ReconciliationPending + bounded recheck (open-orders→position→retry→Flat), 9 test (`a068dc5`) |
| Cycle-ici kümülatif risk | CycleProjection.check_and_commit; 6 test (`00137e7`) — kabul→commit, red→eklenmez, exit→eklenmez |
| Trade-level portfolio bütünlüğü | PortfolioHistory; 1 trade=1 outcome; restart çift-yükleme yasağı (replace_all); unknown PnL sahte 0R yazılamaz; 5 test (`d40eeb0`) |
| Fiyat precision tek merkez | perp_price/perp_price_verified; tüm giden emirler (SL/TP/flatten/exit/modify) aynı kural; 3 api test (`27374da`) |
| Secret güvenliği | `.env` git'te tracked değil; kaynakta gerçek key yok (yalnız test dummy `0x1111…`); git geçmişinde credential deseni bulunamadı |
| Leverage başarısız → entry yok | executor entry önü leverage gate; SL/TP/flatten bu gate'ten bağımsız (engine testleri) |

## FAIL (gerçek bug'lar — bu oturumda bulunan)

| # | Dosya | Bulgu | Risk |
|---|---|---|---|
| B1 | `engine/src/bin/bot.rs:228` | kullanıcı-event WS (orderUpdates+userEvents = **fill geri beslemesi**) task'i `.await.ok();` ile ölünce SESSİZ ölüyordu; kill-switch saymıyor; bot fillsiz devam ederdi | HIGH |
| B2 | `data/src/ingestor.rs` (shard spawn) | shard kalıcı ölümü sadece 1 log + respawn YOK → veri akışında kalıcı sessiz boşluk | HIGH |
| B3 | `engine/src/executor.rs` cancel_tp/cancel_sl | `let _ = self.cancel(...)` + başarısızlıkta bile "iptal edildi" YALAN logu → borsada kalıntı TP/SL görünmez kalırdı | MEDIUM |
| B4 | `data/src/ingestor.rs` (universe) | bozuk `dayNtlVlm` → 0.0 → coin SESSİZ evren dışı; görünürlük yok | MEDIUM |

## FIXED

- B1 → supervisor döngü (ölüm loglanır + 5 sn sonra respawn) `abbe957`
- B2 → shard supervisor döngü (aynı desen) `abbe957`
- B3 → Ok/Err ayrımlı log; Err = "İPTAL EDİLEMEDİ — kalıntı emir riski, manuel kontrol" `48ca304`
- B4 → parse hatası/ctx yok ayrımlı `[UNIVERSE]` warn; davranış fail-closed olarak korundu (sadece görünürlük eklendi) `abbe957`

## REGRESSION TESTS

- `supervisor_tests::kullanici_akis_spawn_ok_yutma_yasak` — `.ok()` deseni kaynağa geri gelemez
- `supervisor_tests::kullanici_akis_supervisor_dongusu_mevcut` — supervisor döngüsü + 5 sn bekleyme varlığı
- `num_sozlesme_testleri::shard_supervisor_dongusu_mevcut` — shard supervisor döngüsü varlığı
- `tick_tests::cancel_sonucu_yutulamaz` — `let _ = self.cancel` yasağı + "İPTAL EDİLEMEDİ" logu zorunlu

## UNVERIFIED (dış sistem / bu oturumda çalıştırılmadı)

- **fmt/clippy**: toolchain'de kurulu değil → `cargo fmt --check` / `clippy -D warnings` ÇALIŞTIRILAMADI. Warning'ler `cargo test/build` derleyici çıktısından sınıflandı: ~20 uyarı — tamamı test kodundaki kullanılmayan import / `mut` / dead-code; üretim mantık hatası işareti YOK. (Kesin sınıflandırma için clippy kurulumu gerekli.)
- **Coverage / mutation** (§34): llvm-cov/mutation kurulu değil → koşturulmadı.
- **Fuzz** (§28): ayrı fuzz hedefi yok; parser dayanıklılığı örnek-bazlı testlerle (malformed JSON/Null/non-finite) kapsanıyor; sistematik fuzz koşusu YAPILMADI.
- **Property-based** (§27): deterministik invariant testleri var (risk/recon/portfolio/precision) ama proptest/quickcheck entegrasyonu YOK.
- **Gerçek exchange davranışları**: partial fill, delayed/lost response, 429, connection-reset CANLI BORSA ÜZERİNDE doğrulanamadı (mock-HTTP testleri bu durumları sentetik üretiyor; canlı server-emulator testi kurulmadı). UNVERIFIED.
- **Ekonomik kârlılık**: CI/test hiçbir koşulda "profitable" kanıtlamaz. Learning epoch (38/100) istatistiksel örneklem değildir.

## RISK RANKING

- CRITICAL: (bu oturumda açık kalan yok)
- HIGH: düzeltildi (B1, B2) — kalıntı risk: supervisor'un kapsamadığı üçüncü-parti task ölümleri (şu an bilinen spawn: bot 1 + ingestor shard'ları; kapsamda)
- MEDIUM: düzeltildi (B3, B4)
- LOW: signing.rs response-text `unwrap_or_default` → non-200'de tanı bilgisini yitirir (diagnostik eksik; sessiz başarı ÜRETMEZ — status 200 + boş text → JSON parse Err, yol açığa çıkar)
- LOW: `bot.rs` NTP/interval gibi diğer best-effort `.ok()` noktaları — her biri gözden geçirilmeli (kalan adet: refresh_positions best-effort'lar)

## FILE-BY-FILE FINDINGS (kritik)

1. **engine/src/bin/bot.rs** — `ws.run(subs, tx).await.ok();` → supervisor. Test: supervisor_tests ×2.
2. **data/src/ingestor.rs** — shard spawn `if let Err` → supervisor döngü. Test: shard_supervisor_dongusu_mevcut. Universe vol: parse-err log.
3. **engine/src/executor.rs** — cancel_tp/cancel_sl `let _` → match log. Test: cancel_sonucu_yutulamaz.
4. **data/src/storage.rs** — (önceki oturum) exec retry→Err; satır red sözleşmeleri; 9 test. Bu oturumda yeni bulgu yok.
5. **api/src/signing.rs** — `resp.text().unwrap_or_default()` → LOW (diagnostik); status≠200 yolu zaten Err.

## FINAL SYSTEM FLOW (gerçek kod davranışı)

DATA (WS shard supervisor'lı + REST backfill, num()=Option sözleşmesi)
→ STRATEGY (15 kanonik strateji, missing data → Skip/Hold)
→ ENSEMBLE (min_score 0.30, TrendBoost clamp, Exit skoru ayrı korunur)
→ REGIME (tek kaynak `decide()` ağacı; Unknown → fail-closed)
→ RISK (CycleProjection: cycle-ici kümülatif; peak-equity; halt)
→ EXECUTION (leverage gate → entry; precision = perp_price merkez kural; cancel hatası görünür)
→ EXCHANGE RESPONSE (accept/reject/UNKNOWN → ReconciliationPending evidence loop; duplicate-entry gate)
→ STATE MACHINE (geçersiz geçiş guard'ları)
→ TRACKING (trade-level portfolio; unknown PnL sahte 0R yazılamaz; restart çift-yazı yasak)
→ LEARNING (Kelly yalnız gerçek portfolio outcome)

## Sonuç

Bu oturumda 4 gerçek bug (2 HIGH, 2 MEDIUM) bulunup düzeltildi; her biri regresyon testiyle korundu. "System is fully correct" veya "production ready" iddiası YOK — kanıtlanamayan alanlar yukarıda UNVERIFIED olarak işaretlendi.
