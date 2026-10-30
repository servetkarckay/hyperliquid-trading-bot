# Katkı Rehberi

Bu projeye katkı yapmak istediğiniz için teşekkürler. Aşağıdaki kurallar katkıların tutarlı ve güvenli olmasını sağlar.

## Genel kurallar

- **Rust edition 2021**, `cargo fmt` + `cargo clippy --workspace --all-targets --all-features -- -D warnings` temiz olmalı.
- Tüm testler geçmeli: `cargo test --workspace`.
- Yorumlar ve commit mesajları Türkçe yazılır (proje konvansiyonu).
- Commit mesajları kısa ve açıklayıcı: `alan: ne değişti` (ör. `engine: reconcile Unknown durumunda Flat YASAK`).

## Geliştirme akışı

1. Fork → branch (`feat/...` veya `fix/...`)
2. Değişikliği yap, test ekle/güncelle
3. `cargo fmt && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace`
4. PR aç — şablonu doldur (`.github/PULL_REQUEST_TEMPLATE.md`)

## Kritik kurallar (asla ihlal edilmez)

- **Secret sızıntısı yok**: `.env`, `config/*.json`, `config/*.jsonl` repoya girmez. PR'da secret varsa PR reddedilir.
- **REST başarısızlığı "veri yok" kanıtı olamaz**: endpoint hataları `Err` döner, asla `Ok(empty)` sanılmaz.
- **Bozuk veri sessizce sıfıra dönüşmez**: pozisyon/risk alanları zorunlu ve doğrulanmış; eksik → Err.
- **Strateji emir göndermez**: `Strategy::evaluate` yalnız `Signal` döner; emir executor katmanındadır.
- **Replay ↔ canlı hizası**: canlı tarama yalnız kapanmış mum kullanır; replay aynı zinciri kullanır.

## Strateji değişiklikleri

MRE stratejisine (6 katman) dokunulacaksa:
- Hem `plan_tuned` (canlı) hem `replay_chain` (replay) aynı değişikliği alır.
- Replay koşusuyla doğrulanır: `cargo run -p strategy --bin mre_replay -- <candles.json> <horizon>`.
- Yeni katman eklenecekse test + out-of-sample teyit gerekli.

## Testnet doğrulama

Gerçek emir akışı değişikliklerinde testnet-e2e workflow'u çalıştırılır:
`gh workflow run testnet-e2e.yml -f confirm_testnet=TESTNET`

## Lisans

Katkılar MIT lisansı altında yayınlanır.