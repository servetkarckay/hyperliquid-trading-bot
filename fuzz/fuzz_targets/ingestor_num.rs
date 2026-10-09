#![no_main]
//! FUZZ TARGET: `data::ingestor::num` — bayt akışı → JSON parse → sözleşme.
//! Aranan: PANIK YOK · NaN/Inf sızıntı yok · sahte-0 (non-numeric → Some(0.0)) yok.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Ok(s) = std::str::from_utf8(bytes) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else { return };
    let out = data::ingestor::num(&v);
    // ÜRETİM SÖZLEŞMESİ (birebir): Some ⇔ (Number(finite)) ∨ (String→parse(finite)).
    // String sayı ("0" dahil) MEŞRU sıfırdır; sahte-0 = eksik/geçersizden 0.0 üretmek.
    let expected = match &v {
        serde_json::Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        serde_json::Value::String(s) => s.parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    };
    assert_eq!(out, expected, "num() sözleşme sapması: {:?}", v);
});
