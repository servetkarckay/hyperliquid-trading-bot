# CI Audit Summary — Round 2

- commit: `1832a3e` · tarih: 2026-10-08

## Sonuçlar

```text
cargo test --workspace:      316 passed / 0 FAILED   (+10 yeni R2 testi)
cargo build --release:       0 error
cargo fmt --all -- --check:  temiz
cargo clippy:                mekanik 0 · yapısal 41 borç (api crate)
coverage/mutation:           koşturulmadı (araç yok) — UNVERIFIED
fuzz:                        libFuzzer yok; 20k randomized fallback ✓
```

## Bu turda bulunan ve düzeltilen buglar

| Seviye | Bug | Fix |
|---|---|---|
| CRITICAL | GİRİŞ ÖLÜ-KİLİDİ: acts-loop aynı cycle kararını kendisi blokluyordu → 51 coin sonsuz Pending{oid:0}, batch ASLA gönderilmez | `c7ec0de`: pre_pending cycle-başı görüntüsü + oid=0 grace + 4 test; canlı kanıt: fix sonrası 7 order gönderildi, fill + SL/TP kurumada + epoch 0→32/100 |
| HIGH | ClickHouse sunucusu kapalı (arşiv 8123) | server 25.8.34 yeniden kuruldu; start_bot.sh ön-sağlık + otomatik başlatma |
| MEDIUM | squeeze dead-guard (`win_start<0` usize) | checked_sub; 125..149→Skip sınır testi |
| MEDIUM | 33× NaN-dönüşmez `!(x>t)` | tekdüze eşdeğerlbiçi (NaN aynı) |
| MEDIUM | linalg/Cholesky/portfolio döngü faktörümüz | satır-boyatma rewrites |

## CI

- `.github/workflows/pr-ci.yml` — PR/push: fmt + clippy (borç lu): -A `result_large_err`,`too_many_arguments` (|| true kaldırılacak) + test + release build
- `.github/workflows/testnet-e2e.yml` — ONLY manual dispatch; "TESTNET" onay; mainnet secret/env guard; probe_smoke binary (limit → open-before-place → cancel → flat); timeout/20 min

## Kalan borçlar (öncelik sırası)

1. api crate `result_large_err` boxing refactor (38) → sonra `pr-ci.yml`'den `-A ...|| true` kaldırılır
2. exchange_endpoint `too_many_arguments` param-styla (3)
3. Coverage/mutation CI adımı (llvm-cov install)
4. libFuzzer nightly hedefi (isteyen ilerleyiği)
5. E2E 20-senaryo tam matrisi (canlı kanıt kısmi örtüşmedi)

## UNVERIFIED

Coverage % · mutation survivors · libFuzzer gerçek koşu · canlı borsa değişken davranışlar (partial-fill/delayed/429 davranış-öngör) · kâr/performans claim'leri — kanıt YOK, iddia YOK.
