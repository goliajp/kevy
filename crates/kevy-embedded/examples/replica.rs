//! `kevy-embedded` as a read-replica of a `kevy` server.
//!
//! This is the Phase 2 (v1.20+) "embed ↔ server" topology in its
//! simplest shape: an application embeds the read-side keyspace in-
//! process while keeping the source-of-truth on a kevy server, and
//! reads pay zero network round-trip.
//!
//! ## Setup
//!
//! Bring up a kevy server with a replication listener on, say,
//! `127.0.0.1:16004`:
//!
//! ```sh
//! kevy --port 6004 --replication-listener 16004
//! ```
//!
//! Then run this example:
//!
//! ```sh
//! cargo run -p kevy-embedded --example replica --release -- 127.0.0.1:16004
//! ```
//!
//! The example connects, mirrors writes the server applies, and
//! polls a few keys for ~5 seconds to show the catch-up. To produce
//! traffic on the primary, drive it from any kevy client in another
//! terminal:
//!
//! ```sh
//! kevy-cli -p 6004 SET hello world
//! kevy-cli -p 6004 INCR counter
//! ```

use std::time::{Duration, Instant};

use kevy_embedded::Store;

fn main() {
    let upstream = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:16004".to_string());

    println!("kevy-embedded replica → upstream {upstream}");

    let replica = Store::open_replica(&upstream).expect("failed to construct replica store");
    assert!(replica.is_replica());

    // Try writing — should be rejected with READONLY.
    let err = replica.set(b"local", b"nope").expect_err("expected READONLY");
    println!("local write refused as expected: {err}");

    // Poll a handful of keys; whatever the primary's traffic puts there
    // will show up here within a tick.
    let watch: [&[u8]; 4] = [b"hello", b"counter", b"key1", b"foo"];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        for k in watch {
            if let Some(v) = replica.get(k).unwrap() {
                println!("replica saw {:?} = {:?}", std::str::from_utf8(k).unwrap(), v);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
