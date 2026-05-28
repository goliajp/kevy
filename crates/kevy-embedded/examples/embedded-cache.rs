//! Embedded LRU cache with a hard memory ceiling — the canonical
//! "in-process Redis cache" use case. Stores 10 000 entries against a
//! 200 KiB ceiling; the LRU policy evicts the oldest to make room.
//!
//! Run with: `cargo run -p kevy-embedded --example embedded-cache`

use kevy_embedded::{Config, EvictionPolicy, Store};

fn main() -> std::io::Result<()> {
    let s = Store::open(
        Config::default()
            .with_max_memory(200 * 1024)
            .with_eviction(EvictionPolicy::AllKeysLru),
    )?;

    for i in 0..10_000 {
        let key = format!("user:{i:05}");
        let val = format!("user-payload-{i}");
        s.set(key.as_bytes(), val.as_bytes())?;
    }

    println!("dbsize after insert flood: {}", s.dbsize());
    println!("used_memory: {} bytes (limit 200 KiB)", s.used_memory());
    println!("evictions_total: {}", s.evictions_total());

    // Touch a recent key — it should still be live.
    let recent = format!("user:0{}", 9999);
    println!(
        "user:09999 → {:?}",
        s.get(recent.as_bytes())?.as_deref().map(String::from_utf8_lossy)
    );

    Ok(())
}
