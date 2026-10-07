# CI AUDIT SUMMARY

- commit: `48ca304` · date: 2026-10-07

## Sonuç sayıları

```text
cargo test --workspace:            305 passed / 0 failed
cargo build --release --workspace: 0 error
cargo fmt --check:                 ÇALIŞTIRILAMADI (rustfmt toolchain'de yok)
cargo clippy -D warnings:          ÇALIŞTIRILAMADI (clippy toolchain'de yok)
coverage/mutation/fuzz:            koşturulmadı (araç kurulu değil)
```

## Bu oturumda bulunan ve düzeltilen bug'lar

| Seviye | Bug | Fix | Test |
|---|---|---|---|
| HIGH | Kullanıcı-event WS task ölümü `.ok()` ile sessiz yutuluyordu (fill geri beslemesi kalıcı kaybolur, kill-switch saymaz) | supervisor döngü: log + 5 sn respawn (`abbe957`) | supervisor_tests ×2 |
| HIGH | Ingestor shard kalıcı ölüm → respawn yok → sessiz veri boşluğu | shard supervisor döngü (`abbe957`) | shard_supervisor_dongusu_mevcut |
| MEDIUM | cancel_tp/cancel_sl sonucu yutuluyor + başarısızlıkta yalan "iptal edildi" logu | Ok/Err ayrımlı log, Err'de kalıntı-emir uyarısı (`48ca304`) | cancel_sonucu_yutulamaz |
| MEDIUM | Bozuk dayNtlVlm → coin sessiz evren dışı | `[UNIVERSE]` ayrımlı warn, davranış fail-closed kalır (`abbe957`) | (log görünürlüğü; davranış testi evren testlerinde) |

## Kalan borçlar (öncelik sırası)

1. CI pipeline kur (fmt+clippy kurulumu dahil) — bugünkü warning'ler o zaman -D warnings'e tabi olur
2. E2E 20-senaryo mock-exchange matrisi (TEST_MATRIX §32 kısmi)
3. Property/fuzz katmanı (§27-28)
4. `signing.rs` response-text diagnostik iyileştirmesi (LOW)
5. Best-effort `.ok()` noktalarının tam envanteri (LOW)

## UNVERIFIED

Canlı borsa davranışları (partial fill, delayed/lost response, 429, reset), ekonomik kârlılık, coverage metrikleri — kanıt YOK, iddia YOK.
