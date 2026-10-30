# Güvenlik Politikası

## Desteklenen sürümler

Yalnız `master` branch'i aktif olarak desteklenir.

## Güvenlik açığı bildirimi

Bir güvenlik açığı bulduysanız lütfen **public issue açmayın**. Bunun yerine:

1. GitHub Security Advisories üzerinden private report açın: https://github.com/servetkarckay/hyperliquid-trading-bot/security/advisories/new
2. Veya maintainer'a e-posta gönderin: servetkarckay@gmail.com

Raporunuzda şunları belirtin:
- Etkilenen bileşen (crate/modül)
- Yeniden üretim adımları
- Etki değerlendirmesi (fon kaybı riski, veri sızıntısı vb.)

## Yanıt süresi

- 48 saat içinde alındı onayı
- 7 gün içinde ilk değerlendirme
- Kritik açıklar için acil düzeltme + güvenlik uyarısı

## Güvenlik ilkeleri (kod tabanı)

Bu bot gerçek para ile çalışır. Kod tabanındaki güvenlik ilkeleri:

- **REST başarısızlığı asla "veri yok" kanıtı olamaz** — endpoint hataları `Err` döner; `unwrap_or_default()` yasak.
- **Bozuk veri sessizce sıfıra dönüşmez** — pozisyon/risk alanları zorunlu ve doğrulanmış.
- **Corrupt risk state → mainnet başlangıcı reddedilir** (bail).
- **Secret'lar asla log'a sızmaz** — `Debug` impl private_key'i maskeler.
- **`.env` ve `config/*.json` repoya girmez** (`.gitignore`).
- **Testnet E2E** her değişiklikte gerçek emir akışını doğrular.

## Bilinen güvenlik sınırları

- Private key rotation operatör aksiyonu (bot çalışırken anahtar değişmez).
- Bot tek makinede çalışır; multi-instance koordinasyonu yok.