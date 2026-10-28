//! Build script — `git_sha`'yi derleme anında enjekte eder.
//!
//! `trade_exits.jsonl` kayıtlarındaki `git` alanı böylece hangi commit'in
//! koduyla kapanıldığını gösterir ("unknown" yerine). `GIT_SHA` env'i
//! önceliklidir (arşiv derlemesi için); git yoksa "unknown" kalır.
use std::process::Command;

fn main() {
    let sha = std::env::var("GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        });
    println!(
        "cargo:rustc-env=GIT_SHA={}",
        sha.as_deref().unwrap_or("unknown")
    );
    // HEAD değişince yeniden derle (izlenmeyen dosya: commit'ler arası geçiş)
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-env-changed=GIT_SHA");
}
