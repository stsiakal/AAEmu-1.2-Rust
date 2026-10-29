//! Copies the stock `aaemu-game` main into `OUT_DIR` with its module-level `//!` docs turned into
//! ordinary comments. `include!` cannot carry inner doc comments, and keeping the stock file as
//! the single source means upstream edits to `aaemu-game/src/main.rs` flow into
//! `aaemu-game-bots` unchanged.
use std::path::PathBuf;

fn main() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../aaemu-game/src/main.rs");
    println!("cargo:rerun-if-changed={}", src.display());
    let text = std::fs::read_to_string(&src).expect("reading aaemu-game/src/main.rs");
    let mut stripped: String = text
        .lines()
        .map(|line| match line.trim_start().strip_prefix("//!") {
            Some(rest) => format!("//{rest}"),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    stripped.push('\n');
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("stock_main.rs");
    std::fs::write(out, stripped).expect("writing stock_main.rs");
}
