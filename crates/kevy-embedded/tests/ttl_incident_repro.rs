//! Repro for INC-2026-06-09 (mailrs): kevy_embedded set_with_ttl TTL not honored.
//! Manual reaper everywhere (no background thread) + lazy reap on get.

use kevy_embedded::{Config, Store};
use std::time::Duration;

/// T1: in-memory TTL via set_with_ttl honors after sleep past expiry.
#[test]
fn t1_ttl_in_memory_expires() {
    let s = Store::open(Config::default().with_ttl_reaper_manual()).unwrap();
    s.set_with_ttl(b"k", b"v", Duration::from_millis(300)).unwrap();
    assert_eq!(s.get(b"k").unwrap(), Some(b"v".to_vec()));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(s.get(b"k").unwrap(), None, "T1: TTL not honored");
}

/// T3: TTL survives an AOF restart and STILL expires at the *original* deadline.
#[test]
fn t3_ttl_survives_restart_and_still_expires() {
    // Use a per-run unique dir so re-running the test on a CI machine
    // that has a stale `/tmp/kevy_ttl_repro_t3_unique` from a prior run
    // (potentially owned by a different user) doesn't fail with
    // PermissionDenied. `nanos` is good enough for in-test uniqueness.
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("kevy_ttl_repro_t3_{uniq}_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();

    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_ttl_reaper_manual(),
        )
        .unwrap();
        s.set_with_ttl(b"k", b"v", Duration::from_millis(800)).unwrap();
        assert_eq!(s.get(b"k").unwrap(), Some(b"v".to_vec()));
    } // drop -> flush AOF

    // Age the key 600ms of its 800ms life, then "restart" (reopen -> AOF replay).
    std::thread::sleep(Duration::from_millis(600));
    let s = Store::open(
        Config::default().with_persist(&dir).with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(s.get(b"k").unwrap(), Some(b"v".to_vec()), "value lost after restart");
    let pttl = s.ttl_ms(b"k");
    eprintln!("T3: pttl right after restart = {pttl} ms (original deadline ~200ms away)");

    // Original expiry was 800ms from first set; we already slept 600ms. A
    // further 400ms is past the ORIGINAL expiry. If replay re-extended the TTL
    // to restart+800ms, the key is still alive here -> bug.
    std::thread::sleep(Duration::from_millis(400));
    let after = s.get(b"k").unwrap();
    eprintln!("T3: get after original-expiry = {after:?}");
    assert_eq!(
        after, None,
        "T3 BUG: key alive past ORIGINAL expiry (TTL re-extended to restart+full on AOF replay)"
    );
}
