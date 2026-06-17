//! Reproducer: open a `Store` whose data_dir contains a real prod AOF.
//!
//! Usage:
//!     cargo run --example replay_real_aof --release -- <path-to-aof>
//!
//! Stages the AOF into a temp dir under the expected `aof-0.aof` name,
//! then runs the full `Store::open` path so the panic (if any) fires
//! at the genuine call site rather than a stubbed mock.

use std::path::PathBuf;

use kevy_embedded::{Config, Store};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: replay_real_aof <aof-path>");
    let src = PathBuf::from(&path);
    let bytes = std::fs::metadata(&src).map_or(0, |m| m.len());
    println!("reproducer: staging {} bytes from {}", bytes, src.display());

    let dir = std::env::temp_dir().join(format!(
        "kevy-aof-reproducer-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let staged = dir.join("aof-0.aof");
    std::fs::copy(&src, &staged).expect("copy AOF into staging dir");

    println!("reproducer: opening Store from {}", dir.display());
    let store = Store::open(
        Config::default()
            .with_persist(&dir)
            .with_aof_filename("aof-0.aof"),
    )
    .expect("Store::open");

    println!("reproducer: opened OK; dbsize = {}", store.dbsize());
}
