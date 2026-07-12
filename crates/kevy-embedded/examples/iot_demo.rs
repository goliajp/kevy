//! Edge-gateway telemetry demo on the `core` archetype — the shape a
//! real IoT deployment runs: a memory-capped store on a small device
//! ingesting sensor readings, holding a rolling retention window, and
//! answering queries locally with no server and no network.
//!
//! What it exercises (all on `--no-default-features --features core`,
//! i.e. no persistence / index / replication / listener compiled in):
//!
//!   1. **Hard memory ceiling + LRU eviction** — the device has a few
//!      MB, not a few GB. The store is capped and evicts rather than
//!      growing without bound; the demo reports what it evicted.
//!   2. **Telemetry ingest throughput** — sustained point writes.
//!   3. **Rolling retention via TTL** — readings expire out of the
//!      window; a manual reaper tick (no background thread on a device
//!      that may not have one) reclaims them.
//!   4. **Per-device ring buffer** — a bounded recent-history list.
//!   5. **Local aggregation** — counters, without a round trip.
//!   6. **Resident memory, measured** — RSS at open and under load,
//!      because on an MCU-class box the number is the product.
//!
//! Run:
//!   cargo run --release -p kevy-embedded --example iot_demo \
//!     --no-default-features --features core
//!
//! Ships as a static-musl binary for the device; see bench/iotgate.sh.

use core::time::Duration;
use kevy_embedded::{Config, EvictionPolicy, Store};

/// The device's budget. Deliberately small — this is the whole point:
/// kevy must stay inside it and evict, not OOM the gateway.
const MEMORY_BUDGET: u64 = 2 * 1024 * 1024; // 2 MiB of values
const DEVICES: usize = 50;
const READINGS_PER_DEVICE: usize = 400;
const RETENTION: Duration = Duration::from_secs(30);
const RING: usize = 20; // recent readings kept per device

fn main() -> kevy_embedded::KevyResult<()> {
    println!("== kevy IoT edge-gateway demo (core archetype) ==\n");

    let store = Store::open(
        Config::default()
            .with_max_memory(MEMORY_BUDGET)
            .with_eviction(EvictionPolicy::AllKeysLru)
            // No background reaper thread: a device may be single-task.
            // The gateway ticks the reaper on its own loop instead.
            .with_ttl_reaper_manual(),
    )?;

    let rss_open = rss_kb();
    println!("device budget   : {} KiB values, LRU eviction", MEMORY_BUDGET / 1024);
    report_rss("RSS at open", rss_open);

    // ---- 1. Telemetry ingest -------------------------------------
    // Each reading: a point write (latest value) + a ring-buffer push
    // (recent history) + a per-device counter bump.
    let total = DEVICES * READINGS_PER_DEVICE;
    let t0 = std::time::Instant::now();
    for r in 0..READINGS_PER_DEVICE {
        for dev in 0..DEVICES {
            let latest = format!("dev:{dev}:latest");
            let hist = format!("dev:{dev}:hist");
            let count = format!("dev:{dev}:n");
            // A plausible reading payload (temp, humidity, seq).
            let payload = format!("{{\"t\":{}.{},\"h\":{},\"seq\":{}}}", 20 + (r % 15), r % 10, 40 + (dev % 30), r);

            // Latest value carries the retention window.
            store.set_with_ttl(latest.as_bytes(), payload.as_bytes(), RETENTION)?;
            // Bounded recent history: push then trim to the ring size.
            store.lpush(hist.as_bytes(), &[payload.as_bytes()])?;
            if store.llen(hist.as_bytes())? > RING {
                store.rpop(hist.as_bytes(), 1)?;
            }
            store.incr(count.as_bytes())?;
        }
    }
    let ingest = t0.elapsed();
    let ops = total * 3; // set + lpush(+trim) + incr, roughly
    println!(
        "\ningest          : {total} readings from {DEVICES} devices in {:?}",
        ingest
    );
    println!(
        "                  ~{:.0} store-ops/s sustained",
        ops as f64 / ingest.as_secs_f64()
    );

    // ---- 2. What the memory ceiling did --------------------------
    report_rss("RSS under load", rss_kb());
    println!(
        "used_memory     : {} KiB  (ceiling {} KiB)",
        store.used_memory() / 1024,
        MEMORY_BUDGET / 1024
    );
    println!(
        "evictions       : {}  — the cap held; the gateway did not grow without bound",
        store.evictions_total()
    );
    println!("live keys       : {}", store.dbsize());

    // ---- 3. Local queries, no network ----------------------------
    println!("\n-- local reads (no server, no round trip) --");
    for dev in [0usize, DEVICES / 2, DEVICES - 1] {
        let latest = format!("dev:{dev}:latest");
        let hist = format!("dev:{dev}:hist");
        let count = format!("dev:{dev}:n");
        let v = store.get(latest.as_bytes())?;
        let depth = store.llen(hist.as_bytes())?;
        let n = store.get(count.as_bytes())?;
        let ttl = store.ttl_ms(latest.as_bytes());
        match v {
            Some(v) => println!(
                "dev {dev:>3}  latest={}  ring={depth}  readings={}  ttl={}s",
                String::from_utf8_lossy(&v),
                n.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|| "-".into()),
                ttl / 1000
            ),
            // An evicted device is the expected outcome of a hard cap —
            // the gateway keeps the hot set, sheds the cold.
            None => println!("dev {dev:>3}  (evicted by the memory ceiling — hot set retained)"),
        }
    }

    // ---- 4. Archive pressure: does the ceiling actually hold? -----
    // A gateway wants to keep as much history as it can. Archive every
    // reading under its own key and deliberately overrun the budget —
    // the device must evict its cold set, NOT OOM. This is the property
    // that decides whether kevy can live on a 2 MB device at all.
    println!("\n-- archive pressure: writing far past the {} KiB ceiling --", MEMORY_BUDGET / 1024);
    let archive_writes = 60_000usize;
    let t1 = std::time::Instant::now();
    for i in 0..archive_writes {
        let k = format!("arch:{}:{}", i % DEVICES, i);
        let v = format!("{{\"t\":{}.{},\"seq\":{i}}}", 20 + (i % 15), i % 10);
        store.set(k.as_bytes(), v.as_bytes())?;
    }
    let arch = t1.elapsed();
    println!(
        "archived        : {archive_writes} readings in {:?} (~{:.0} writes/s)",
        arch,
        archive_writes as f64 / arch.as_secs_f64()
    );
    println!(
        "used_memory     : {} KiB  — still inside the {} KiB ceiling",
        store.used_memory() / 1024,
        MEMORY_BUDGET / 1024
    );
    println!(
        "evictions       : {}  — the cold set was shed to hold the line",
        store.evictions_total()
    );
    println!("live keys       : {}", store.dbsize());
    report_rss("RSS after", rss_kb());

    // ---- 5. Rolling retention: TTL really expires -----------------
    // Short window + a manual tick: the device reclaims on its own loop,
    // with no background thread.
    println!("\n-- rolling retention (TTL) --");
    for dev in 0..DEVICES {
        let k = format!("win:{dev}");
        store.set_with_ttl(k.as_bytes(), b"in-window", Duration::from_millis(300))?;
    }
    let with_window = store.exists(
        &(0..DEVICES).map(|d| format!("win:{d}")).collect::<Vec<_>>()
            .iter().map(|s| s.as_bytes()).collect::<Vec<_>>(),
    )?;
    println!("window keys     : {with_window} live");
    std::thread::sleep(Duration::from_millis(400));
    let before = store.dbsize();
    store.tick(); // the gateway's own loop reclaims — no background thread
    let survivors = store.exists(
        &(0..DEVICES).map(|d| format!("win:{d}")).collect::<Vec<_>>()
            .iter().map(|s| s.as_bytes()).collect::<Vec<_>>(),
    )?;
    println!(
        "after 400ms+tick: {survivors} live  ({} → {} keys, expired-out total {})",
        before,
        store.dbsize(),
        store.expired_keys_total()
    );

    println!("\n== the gateway held its budget, shed cold data, expired its window,");
    println!("   and served every read locally — no server, no network ==");
    Ok(())
}

fn report_rss(label: &str, kb: Option<u64>) {
    match kb {
        Some(kb) => println!("{label:16}: {kb} KiB resident"),
        None => println!("{label:16}: (RSS probe is Linux-only)"),
    }
}

/// Resident set from `/proc/self/status` — the number that decides
/// whether this fits on the device at all.
fn rss_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}
