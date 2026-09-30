//! A server replica following an embedded primary keeps the catalog the
//! primary records, whatever order its shards' full syncs arrive in.
//! Needs the server crate, which the published package does not carry.

#![cfg(all(feature = "index", feature = "replicate", not(target_arch = "wasm32")))]
#![allow(clippy::unwrap_used, clippy::panic)]

use std::path::Path;

use kevy_embedded::{Config, Store};
use kevy_tmpdir::TmpDir;

fn ok(s: &Store, cmd: &str) {
    let argv: Vec<Vec<u8>> = cmd.split(' ').map(|p| p.as_bytes().to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    assert_eq!(String::from_utf8_lossy(&out), "+OK\r\n", "{cmd}");
}

fn config(dir: &TmpDir, shards: usize) -> Config {
    Config::default().with_persist(dir.path()).with_shards(shards).with_ttl_reaper_manual()
}

/// The catalog frame shard 0's snapshot in `dir` carries.
fn saved_frame(dir: &Path) -> Option<kevy_persist::Argv> {
    let file = std::fs::File::open(dir.join("dump-0.rdb")).unwrap();
    let mut keys = kevy_rt::Store::new();
    kevy_persist::load_snapshot_with_aux(&mut keys, std::io::BufReader::new(file), |_| true)
        .unwrap()
}

/// A server replica of an embedded primary: one of its shards applies the
/// first catalog frame from its stream, then another loads a full sync
/// served from a snapshot taken before any declaration. The catalog stays.
#[test]
fn a_server_replica_keeps_the_catalog_when_an_older_embedded_snapshot_lands_last() {
    use kevy_rt::Commands;
    let dir = TmpDir::new("emb-catalog-older-snapshot");
    let s = Store::open(config(&dir, 1)).unwrap();
    assert!(s.save_snapshot().unwrap());
    let older = saved_frame(dir.path());
    ok(&s, "IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range");
    assert!(s.save_snapshot().unwrap());
    let frame = saved_frame(dir.path()).expect("the declaration was recorded");
    let replica = kevy::KevyCommands::sharded(2);
    {
        let _applying = kevy_rt::RecordApplyGuard::enter();
        assert_eq!(replica.dispatch(&mut kevy_rt::Store::new(), &frame), b"+OK\r\n");
    }
    replica.load_snapshot_aux(older.as_ref(), true);
    let held = replica.snapshot_aux().expect("the replica holds a catalog");
    assert_eq!((&held[1], &held[2]), (&frame[1], &frame[2]));
    assert!(String::from_utf8_lossy(&held[3]).contains("age"));
}
