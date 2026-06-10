//! Sharding (B2) + server-dir-interop tests for the embedded store.
//! Split from `store_tests.rs` to keep both under the 500-LOC house rule.

use super::*;
use super::tests::tmp_dir;
use crate::PubsubFrame;

// ───────────────────────── sharding (B2) ─────────────────────────

#[test]
fn sharded_in_memory_roundtrip() {
    let s = Store::open(Config::default().with_shards(8).with_ttl_reaper_manual()).unwrap();
    for i in 0..1000u32 {
        s.set(format!("k{i}").as_bytes(), format!("v{i}").as_bytes()).unwrap();
    }
    assert_eq!(s.dbsize(), 1000);
    for i in 0..1000u32 {
        assert_eq!(
            s.get(format!("k{i}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
    // Cross-shard DEL: keys hash to different shards.
    let keys: Vec<Vec<u8>> = (0..1000u32).map(|i| format!("k{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
    assert_eq!(s.del(&refs).unwrap(), 1000);
    assert_eq!(s.dbsize(), 0);
}

#[test]
fn sharded_persist_survives_restart() {
    let dir = tmp_dir("sharded-restart");
    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_shards(4).with_ttl_reaper_manual(),
        )
        .unwrap();
        for i in 0..500u32 {
            s.set(format!("k{i}").as_bytes(), format!("v{i}").as_bytes()).unwrap();
        }
        assert_eq!(s.dbsize(), 500);
    }
    // Per-shard AOFs exist; reopen at the same shard count loads them.
    assert!(dir.join("shards.meta").exists());
    assert!(dir.join("aof-3.aof").exists());
    let s2 = Store::open(
        Config::default().with_persist(&dir).with_shards(4).with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(s2.dbsize(), 500);
    assert_eq!(s2.get(b"k0").unwrap(), Some(b"v0".to_vec()));
    assert_eq!(s2.get(b"k499").unwrap(), Some(b"v499".to_vec()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn migrates_single_aof_to_shards() {
    let dir = tmp_dir("migrate");
    // 1. Write with the default single-shard layout (one aof-0.aof, no meta).
    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_ttl_reaper_manual(),
        )
        .unwrap();
        for i in 0..300u32 {
            s.set(format!("k{i}").as_bytes(), format!("v{i}").as_bytes()).unwrap();
        }
    }
    assert!(dir.join("aof-0.aof").exists());
    // Since the dir-interop fix the single-shard open records its layout
    // too (default filenames are server-readable).
    assert!(dir.join("shards.meta").exists());

    // 2. Reopen opting into 4 shards → migrates the single AOF.
    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_shards(4).with_ttl_reaper_manual(),
        )
        .unwrap();
        assert_eq!(s.dbsize(), 300);
        for i in 0..300u32 {
            assert_eq!(
                s.get(format!("k{i}").as_bytes()).unwrap(),
                Some(format!("v{i}").into_bytes())
            );
        }
    }
    // Migration artifacts: meta written, legacy AOF backed up, per-shard files.
    assert!(dir.join("shards.meta").exists());
    assert!(std::fs::read_dir(&dir).unwrap().any(|e| {
        e.unwrap().file_name().to_string_lossy().contains("premigration")
    }));

    // 3. Reopen sharded again → loads per-shard, data intact.
    let s3 = Store::open(
        Config::default().with_persist(&dir).with_shards(4).with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(s3.dbsize(), 300);
    assert_eq!(s3.get(b"k150").unwrap(), Some(b"v150".to_vec()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sharded_pubsub_still_process_wide() {
    // Pub/sub is on shard 0; publishing reaches a subscriber regardless of
    // how the keyspace is sharded.
    let s = Store::open(Config::default().with_shards(8).with_ttl_reaper_manual()).unwrap();
    let sub = s.subscribe(&[b"chan"]);
    let _ack = sub.recv().unwrap();
    assert_eq!(s.publish(b"chan", b"hello"), 1);
    assert_eq!(
        sub.recv().unwrap(),
        PubsubFrame::Message { channel: b"chan".to_vec(), payload: b"hello".to_vec() }
    );
}

#[test]
fn collect_keys_spans_all_shards() {
    let s = Store::open(Config::default().with_shards(8).with_ttl_reaper_manual()).unwrap();
    for i in 0..500u32 {
        s.set(format!("user:{i}").as_bytes(), b"v").unwrap();
    }
    for i in 0..50u32 {
        s.set(format!("other:{i}").as_bytes(), b"v").unwrap();
    }
    // Glob scan must span ALL shards, not just shard 0 (the `with` hole).
    let matched = s.collect_keys(Some(b"user:*"), None);
    assert_eq!(matched.len(), 500);
    // limit bounds the total across shards.
    assert_eq!(s.collect_keys(Some(b"user:*"), Some(100)).len(), 100);
    // for_each_shard sees the whole keyspace.
    let mut total = 0;
    s.for_each_shard(|inner| total += inner.dbsize());
    assert_eq!(total, 550);
    assert_eq!(s.shard_count(), 8);
}

// ──────────────── dir interop with the server (2026-06-11) ────────────────

/// A meta-less multi-shard dir (e.g. written by a pre-meta server) used to
/// be mistaken for the single-file layout: an n==1 open silently loaded
/// shard 0 only — (k-1)/k of the keyspace gone. Now the file names are
/// inferred and the dir is migrated whole.
#[test]
fn metaless_multishard_dir_is_migrated_not_partially_loaded() {
    let dir = tmp_dir("metaless-multi");
    std::fs::create_dir_all(&dir).unwrap();
    // Hand-build two shard snapshots, no shards.meta.
    let mut a = kevy_store::Store::new();
    a.set(b"alpha", b"1".to_vec(), None, false, false);
    kevy_persist::save_snapshot(&a, &dir.join("dump-0.rdb")).unwrap();
    let mut b = kevy_store::Store::new();
    b.set(b"beta", b"2".to_vec(), None, false, false);
    kevy_persist::save_snapshot(&b, &dir.join("dump-1.rdb")).unwrap();

    let s = Store::open(
        Config::default().with_persist(&dir).with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(s.dbsize(), 2, "both shards' keys must survive the n=1 open");
    assert_eq!(s.get(b"alpha").unwrap(), Some(b"1".to_vec()));
    assert_eq!(s.get(b"beta").unwrap(), Some(b"2".to_vec()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Default-named single-shard dirs record their layout so a server (or a
/// later n>1 open) reads them without inference.
#[test]
fn single_shard_default_names_record_meta() {
    let dir = tmp_dir("single-meta");
    {
        let s = Store::open(
            Config::default().with_persist(&dir).with_ttl_reaper_manual(),
        )
        .unwrap();
        s.set(b"k", b"v").unwrap();
    }
    assert!(dir.join("shards.meta").exists());
    assert!(dir.join("aof-0.aof").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Custom filenames opt out of dir interop: the files keep their names and
/// no shards.meta is written (a meta would point a server at files that
/// don't exist).
#[test]
fn custom_filenames_stay_metaless() {
    let dir = tmp_dir("custom-names");
    {
        let s = Store::open(
            Config::default()
                .with_persist(&dir)
                .with_aof_filename("my.aof")
                .with_snapshot_filename("my.rdb")
                .with_ttl_reaper_manual(),
        )
        .unwrap();
        s.set(b"k", b"v").unwrap();
    }
    assert!(dir.join("my.aof").exists());
    assert!(!dir.join("shards.meta").exists());
    // Reopen with the same custom names: data intact.
    let s2 = Store::open(
        Config::default()
            .with_persist(&dir)
            .with_aof_filename("my.aof")
            .with_snapshot_filename("my.rdb")
            .with_ttl_reaper_manual(),
    )
    .unwrap();
    assert_eq!(s2.get(b"k").unwrap(), Some(b"v".to_vec()));
    let _ = std::fs::remove_dir_all(&dir);
}
