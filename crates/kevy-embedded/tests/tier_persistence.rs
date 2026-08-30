//! Tiering × persistence integration (capacity arc T4, RFC §2 B10/B11):
//!
//! - **B10** — BGREWRITEAOF / snapshot on a store with cold keys loses
//!   nothing: the serializers stream cold values from the pinned vlog
//!   (zero promotion, counter-asserted), the reopen restores every
//!   value byte-correct, field-TTLs included. The drop between sessions
//!   is a plain drop — no orderly shutdown — the crash shape.
//! - **B11** — a boot whose dataset exceeds the tier budget demotes
//!   inline during replay: the open succeeds, ends under the demote
//!   watermark, and every key still reads back. Same story across a
//!   re-shard (1 → 2 shards) whose merged dataset exceeds the budget.

#![cfg(all(feature = "tier", not(target_arch = "wasm32")))]

use kevy_embedded::{Config, Store};

fn dispatch(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
    out
}

fn tiered_config(dir: &std::path::Path, budget: u64) -> Config {
    Config::default().with_ttl_reaper_manual().with_persist(dir).with_tier_budget(budget)
}

/// One deterministic value per key so a mixed-up restore cannot pass.
fn val_of(i: u32, len: usize) -> Vec<u8> {
    vec![(i % 251) as u8; len]
}

/// B10, rewrite half: force-demote bulk strings + a hash (with a field
/// TTL), BGREWRITEAOF, drop without shutdown, reopen — every value
/// byte-correct, the field TTL intact, zero promotions during the
/// rewrite.
#[test]
fn b10_rewrite_aof_streams_cold_values_losslessly() {
    let dir = kevy_tmpdir::TmpDir::new("tier-b10-rewrite");
    {
        let s = Store::open(tiered_config(dir.path(), u64::MAX)).expect("open tiered");
        s.set(b"hot:k", b"small").unwrap();
        s.set(b"cold:a", &val_of(1, 4096)).unwrap();
        s.set(b"cold:b", &val_of(2, 2500)).unwrap();
        s.hset(b"cold:row", &[(b"name".as_slice(), b"ada".as_slice()), (b"dept", b"eng")]).unwrap();
        assert_eq!(
            dispatch(&s, &[b"HPEXPIRE", b"cold:row", b"3600000", b"FIELDS", b"1", b"name"]),
            b"*1\r\n:1\r\n".to_vec()
        );
        for k in [b"cold:a".as_slice(), b"cold:b", b"cold:row"] {
            assert!(s.debug_force_demote(k), "warm spillable key must demote");
        }

        let (d0, p0) = s.tier_counters();
        s.rewrite_aof().expect("rewrite").expect("stats");
        let (d1, p1) = s.tier_counters();
        assert_eq!(p1, p0, "B10: the rewrite must not promote");
        assert_eq!(d1, d0, "B10: the rewrite must not demote either");
        // Dropped here WITHOUT shutdown — the crash shape.
    }
    {
        let s = Store::open(tiered_config(dir.path(), u64::MAX)).expect("reopen");
        assert_eq!(s.get(b"hot:k").unwrap().unwrap(), b"small");
        assert_eq!(s.get(b"cold:a").unwrap().unwrap(), val_of(1, 4096));
        assert_eq!(s.get(b"cold:b").unwrap().unwrap(), val_of(2, 2500));
        assert_eq!(s.hget(b"cold:row", b"name").unwrap().unwrap(), b"ada");
        assert_eq!(s.hget(b"cold:row", b"dept").unwrap().unwrap(), b"eng");
        let mut fttl: Vec<(Vec<u8>, Vec<u8>, u64)> = Vec::new();
        s.with_key(b"cold:row", |st| {
            st.hash_ttl_each(|k, f, dl| fttl.push((k.to_vec(), f.to_vec(), dl)));
        });
        assert_eq!(fttl.len(), 1, "the field TTL must survive the rewrite");
        assert_eq!((&fttl[0].0[..], &fttl[0].1[..]), (b"cold:row".as_slice(), b"name".as_slice()));
        assert!(fttl[0].2 > kevy_store::now_unix_ms(), "deadline must still be in the future");
    }
}

/// B10, snapshot half: SAVE with cold stubs, drop, reopen from the
/// snapshot — byte-correct, zero promotions during the save.
#[test]
fn b10_snapshot_streams_cold_values_losslessly() {
    let dir = kevy_tmpdir::TmpDir::new("tier-b10-snap");
    {
        let s = Store::open(tiered_config(dir.path(), u64::MAX)).expect("open tiered");
        s.set(b"cold:a", &val_of(7, 4096)).unwrap();
        s.hset(b"cold:row", &[(b"f1".as_slice(), b"v1".as_slice())]).unwrap();
        assert!(s.debug_force_demote(b"cold:a"));
        assert!(s.debug_force_demote(b"cold:row"));

        let (d0, p0) = s.tier_counters();
        assert!(s.save_snapshot().expect("save"), "persistence is on");
        let (d1, p1) = s.tier_counters();
        assert_eq!((d1, p1), (d0, p0), "B10: the snapshot must move no tier counter");
    }
    {
        let s = Store::open(tiered_config(dir.path(), u64::MAX)).expect("reopen");
        assert_eq!(s.get(b"cold:a").unwrap().unwrap(), val_of(7, 4096));
        assert_eq!(s.hget(b"cold:row", b"f1").unwrap().unwrap(), b"v1");
    }
}

/// B11: write ~1.5 MB hot under a 128 KiB budget (live writes demote as
/// they go), drop, reopen with the same budget — the replay demotes
/// inline (fresh per-boot counters prove it), ends under the watermark,
/// and every key reads back correctly.
#[test]
fn b11_boot_with_dataset_over_budget_demotes_inline() {
    let dir = kevy_tmpdir::TmpDir::new("tier-b11-boot");
    // Above the stub floor (1500 × ~96 B ≈ 144 KB, the RFC capacity
    // model) — a budget below the floor can never reach the watermark.
    let budget: u64 = 256 * 1024;
    let n: u32 = 1500; // > the 1024-frame demote stride, so the mid-replay check fires
    {
        let s = Store::open(tiered_config(dir.path(), budget)).expect("open tiered");
        for i in 0..n {
            let key = format!("k{i:04}").into_bytes();
            s.set(&key, &val_of(i, 1024)).unwrap();
        }
    }
    {
        let s = Store::open(tiered_config(dir.path(), budget)).expect("reopen over budget");
        let used = s.with(|st| st.used_memory());
        assert!(
            used <= budget * 19 / 20,
            "B11: replay must end under the watermark: {used} > {}",
            budget * 19 / 20
        );
        let (demotions, _) = s.tier_counters();
        assert!(demotions > 0, "B11: the counters are per-boot — replay itself must demote");
        for i in 0..n {
            let key = format!("k{i:04}").into_bytes();
            assert_eq!(s.get(&key).unwrap().unwrap(), val_of(i, 1024), "key {i}");
        }
    }
}

/// B11 across the migration path: reopening 1 → 2 shards re-shards
/// through the temp-merge + redistribution pipeline; with the merged
/// dataset over the budget, the merge demotes inline, the source side
/// materializes cold stubs before shipping, and the targets demote as
/// they fill — nothing lost, nothing OOM-shaped.
#[test]
fn b11_reshard_with_cold_dataset_over_budget() {
    let dir = kevy_tmpdir::TmpDir::new("tier-b11-reshard");
    let budget: u64 = 192 * 1024; // above the 1200-key stub floor (~115 KB)
    let n: u32 = 1200;
    {
        let s = Store::open(tiered_config(dir.path(), budget)).expect("open 1 shard");
        for i in 0..n {
            let key = format!("k{i:04}").into_bytes();
            s.set(&key, &val_of(i, 1024)).unwrap();
        }
    }
    {
        let s = Store::open(tiered_config(dir.path(), budget).with_shards(2))
            .expect("reopen re-sharded over budget");
        let (demotions, _) = s.tier_counters();
        assert!(demotions > 0, "the redistribution targets must have demoted");
        for i in 0..n {
            let key = format!("k{i:04}").into_bytes();
            assert_eq!(s.get(&key).unwrap().unwrap(), val_of(i, 1024), "key {i}");
        }
        let used0 = s.with(|st| st.used_memory());
        assert!(used0 <= budget, "shard 0 must stay bounded: {used0}");
    }
}
