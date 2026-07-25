//! The `core`-tier consumer iotgate sizes and measures: KV + TTL, a
//! manual reaper, nothing else. Prints `rss_kb=<n>` on Linux (VmRSS
//! right after open — the empty-library resident cost) so the gate can
//! assert the memory budget on the artifact a device actually ships.

use kevy_embedded::{Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(Config::default().with_ttl_reaper_manual())?;
    print_rss();

    store.set(b"sensor:1", b"22.5")?;
    debug_assert_eq!(store.get(b"sensor:1")?, Some(b"22.5".to_vec()));
    store.set(b"sensor:2", b"3.3")?;
    store.expire(b"sensor:2", core::time::Duration::from_secs(60))?;
    store.tick();
    println!("keys={}", store.dbsize());
    Ok(())
}

fn print_rss() {
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        if let Some(kb) = s
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
        {
            println!("rss_kb={kb}");
        }
    }
}
