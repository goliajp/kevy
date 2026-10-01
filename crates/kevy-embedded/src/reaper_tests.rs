//! The reaper's everysec fsync runs after the shard lock is released.

use super::*;
use crate::config::AppendFsync;
use crate::store::Store;
use std::sync::mpsc;

fn tmp_dir(name: &str) -> std::path::PathBuf {
    kevy_tmpdir::unique_dir(&format!("embedded-{name}"))
}

fn upkeep(shard: &Arc<RwLock<Inner>>) -> TickSync {
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    let tier: TierSpecOpt = None;
    #[cfg(not(all(feature = "tier", not(target_arch = "wasm32"))))]
    let tier: TierSpecOpt = ();
    shard_upkeep(
        shard,
        16,
        1,
        tier,
        1,
        #[cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]
        None,
    )
}

#[test]
fn a_write_lands_while_the_everysec_sync_is_outstanding() {
    let dir = tmp_dir("reaper-offlock");
    let config = Config::default()
        .with_persist(&dir)
        .with_appendfsync(AppendFsync::EverySec)
        .with_ttl_reaper_manual();
    let store = Store::open(config.clone()).unwrap();
    store.set(b"before", &[b'a'; 4096]).unwrap();
    // the everysec window is one second from open; waiting it out is a
    // lower bound, not a race
    std::thread::sleep(Duration::from_millis(1100));
    let sync = upkeep(&store.shards[0]).expect("the window elapsed with a dirty log");

    // a write on the same shard while the fsync is started but not run:
    // under the old shape the tick held the write lock across the fsync
    let (tx, rx) = mpsc::channel();
    let writer = store.clone();
    std::thread::spawn(move || {
        writer.set(b"during", &[b'b'; 4096]).unwrap();
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(10)).expect("the write must not wait for the fsync");
    super::run_tick_sync(Some(sync));
    // the window restarted with that sync, so the next tick owes none
    super::run_tick_sync(upkeep(&store.shards[0]));

    drop(store);
    let reopened = Store::open(config).unwrap();
    assert_eq!(reopened.get(b"before").unwrap(), Some(vec![b'a'; 4096]));
    assert_eq!(reopened.get(b"during").unwrap(), Some(vec![b'b'; 4096]));
    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn closing_a_store_does_not_wait_out_the_reaper_tick() {
    let config = Config::default()
        .with_persist(tmp_dir("reaper-close"))
        .with_reaper_interval(Duration::from_secs(10));
    let store = Store::open(config).unwrap();
    store.set(b"k", b"v").unwrap();
    let t0 = std::time::Instant::now();
    drop(store);
    // a close that sleeps out the tick takes the full ten seconds
    assert!(t0.elapsed() < Duration::from_secs(1), "close took {:?}", t0.elapsed());
}
