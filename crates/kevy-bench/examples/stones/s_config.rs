//! kevy-config baseline: parse a representative full-section document and
//! serialize it back. The document is `Config::default().to_toml_string()`
//! — the fixed emit template covering every section — so the input is
//! self-consistent and tracks the schema as it grows.

use kevy_bench::{bench, black_box};
use kevy_config::Config;

pub fn run() {
    println!("== kevy-config ==");

    let doc = Config::default().to_toml_string();
    let sections = doc.lines().filter(|l| l.starts_with('[')).count();
    println!("  (doc: {} bytes, {} sections)", doc.len(), sections);

    let s = bench(30, 50, || {
        black_box(Config::from_toml_str(black_box(&doc), None).unwrap());
    });
    crate::row("from_toml_str (full-section doc)", s, 1);

    let cfg = Config::from_toml_str(&doc, None).unwrap();
    let s = bench(30, 100, || {
        black_box(black_box(&cfg).to_toml_string());
    });
    crate::row("to_toml_string", s, 1);
    println!();
}
