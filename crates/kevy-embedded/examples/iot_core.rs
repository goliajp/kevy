//! IoT-shape smoke over the `core` archetype surface only — in-memory
//! KV + TTL, manual reaper, no persistence / index / replication /
//! listener. Doubles as the `bench/iotgate.sh` measurement target:
//!
//! - **binary size**: build with
//!   `cargo build --profile iot -p kevy-embedded --example iot_core
//!    --no-default-features --features core` and measure the artifact;
//! - **empty-store RSS** (Linux): the example prints `rss_kb=<n>`
//!   (from `/proc/self/status` VmRSS) right after opening the store,
//!   before any write — the "empty library" resident cost.

use kevy_embedded::{Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    let s = Store::open(Config::default().with_ttl_reaper_manual())?;
    print_rss();

    // Sensor-cache shape: point write / read / TTL'd write / tick.
    s.set(b"sensor:1", b"22.5")?;
    assert_eq!(s.get(b"sensor:1")?, Some(b"22.5".to_vec()));
    s.set(b"sensor:2", b"3.3")?;
    s.expire(b"sensor:2", core::time::Duration::from_secs(60))?;
    s.tick();
    println!("keys={}", s.info().keys);
    Ok(())
}

/// Print `rss_kb=<n>` on Linux (the iotgate RSS probe); elsewhere the
/// line is absent and the gate skips the RSS assertion.
fn print_rss() {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
                println!("rss_kb={kb}");
            }
        }
    }
}
