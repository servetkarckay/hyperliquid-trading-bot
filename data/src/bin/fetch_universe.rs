//! TÜM HL PERP UNIVERSE — 1h mum geçmişi toplama (tarihsel doğrulama
//! tarayıcıları için ham veri değil, ucdan veri).
//!
//! Kullanım: `cargo run -p data --bin fetch_universe -- <cikis.json> [bar_sayisi]`
//! (ötanılpış: 4000 bar ≈ 166 gün). Delisted/simtrapp coinler atlanır;
//! candleSnapshot tek istekte ≤5000 bar; paged (2000'lik) geriye.
use hyperliquid_api::types::Candle;
use hyperliquid_api::Info;
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let out = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "/tmp/opencode/all_coins_candles.json".into());
    let bars: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4000);
    let cfg = hyperliquid_api::Config {
        network: "mainnet".into(),
        api_url: "https://api.hyperliquid.xyz".into(),
        ws_url: "wss://api.hyperliquid.xyz/ws".into(),
        // info uçları imza İSTEMEZ — tet isyanı yer tutucu (kullanılmaz)
        private_key: "0000000000000000000000000000000000000000000000000000000000000001".into(),
        agent_address: String::new(),
    };
    let info = Info::new(cfg)?;
    let meta = info.meta().await?;
    let universe: Vec<String> = meta
        .universe
        .iter()
        .filter(|u| u.isDelisted != Some(true))
        .map(|u| u.name.clone())
        .collect();
    println!("universe: {} coin", universe.len());

    let now = hyperliquid_api::now_ms() as i64;
    let hour_ms: i64 = 3_600_000;
    let start = now - hour_ms * bars as i64;

    let mut map = serde_json::Map::new();
    for (k, coin) in universe.iter().enumerate() {
        let mut candles: Vec<Candle> = Vec::new();
        let mut cursor = start;
        loop {
            let chunk_end = (cursor + hour_ms * 2000).min(now);
            match info.candle_snapshot(coin, "1h", cursor, chunk_end).await {
                Ok(mut raw) => {
                    let before = candles.len();
                    candles.append(&mut raw);
                    if candles.len() == before || chunk_end >= now {
                        break;
                    }
                    cursor = candles.last().map(|c| c.t + 1).unwrap_or(chunk_end);
                }
                Err(e) => {
                    eprintln!("{coin}: sayfa hatası {e}");
                    break;
                }
            }
        }
        let mut rows: Vec<serde_json::Value> = Vec::with_capacity(candles.len());
        // tekille + kronolojik
        let mut last_t = 0i64;
        for c in candles {
            if c.t <= last_t {
                continue;
            }
            last_t = c.t;
            let f = |x: &str| x.parse::<f64>().ok().filter(|v| v.is_finite());
            let (Some(o), Some(h), Some(l), Some(cc)) = (f(&c.o), f(&c.h), f(&c.l), f(&c.c)) else {
                continue;
            };
            rows.push(json!({
                "t": c.t, "o": o, "h": h, "l": l, "c": cc, "v": f(&c.v).unwrap_or(0.0)
            }));
        }
        if rows.len() >= 700 {
            map.insert(coin.clone(), serde_json::Value::Array(rows));
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        if (k + 1) % 25 == 0 {
            println!("{} / {} tamamlandı", k + 1, universe.len());
        }
    }
    println!("ETKİN coin: {}", map.len());
    let txt = serde_json::to_string(&map)?;
    std::fs::write(&out, txt)?;
    println!("kaydedildi: {out}");
    Ok(())
}
