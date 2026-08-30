//! SegHash/SegSet (bucket-sharded COW) tests: promotion, split
//! correctness at scale, per-bucket COW behavior under a pinned
//! snapshot view, semantics vs a model, and accounting round-trips.

use crate::Store;
use crate::seg_map::HS_PROMOTE;
use crate::value::Value;

fn hset_n(st: &mut Store, key: &[u8], n: usize) {
    for i in 0..n {
        let f = alloc::format!("field-{i:08}");
        let v = alloc::format!("value-{i:08}");
        st.hset(key, &[(f.as_bytes(), v.as_bytes())]).unwrap();
    }
}

fn sadd_n(st: &mut Store, key: &[u8], n: usize) {
    for i in 0..n {
        let m = alloc::format!("member-{i:08}");
        st.sadd(key, &[m.as_bytes()]).unwrap();
    }
}

fn is_seghash(st: &Store, key: &[u8]) -> bool {
    matches!(st.map.get(key).map(|e| &e.value), Some(Value::SegHash(_)))
}

fn is_segset(st: &Store, key: &[u8]) -> bool {
    matches!(st.map.get(key).map(|e| &e.value), Some(Value::SegSet(_)))
}

#[test]
fn hash_promotes_and_splits_stay_routable() {
    let mut st = Store::new();
    let n = 3 * HS_PROMOTE; // forces directory doubling + several splits
    hset_n(&mut st, b"h", n);
    assert!(is_seghash(&st, b"h"));
    assert_eq!(st.hlen(b"h").unwrap(), n);
    // Every key must still route to its bucket after all splits.
    for i in (0..n).step_by(997) {
        let f = alloc::format!("field-{i:08}");
        let v = st.hget(b"h", f.as_bytes()).unwrap().expect("present");
        assert_eq!(v, alloc::format!("value-{i:08}").as_bytes());
    }
    assert_eq!(st.hgetall(b"h").unwrap().len(), n * 2);
}

#[test]
fn set_promotes_and_membership_holds() {
    let mut st = Store::new();
    let n = HS_PROMOTE + 500;
    sadd_n(&mut st, b"s", n);
    assert!(is_segset(&st, b"s"));
    assert_eq!(st.scard(b"s").unwrap(), n);
    for i in (0..n).step_by(499) {
        let m = alloc::format!("member-{i:08}");
        assert!(st.sismember(b"s", m.as_bytes()).unwrap());
    }
    assert!(!st.sismember(b"s", b"absent").unwrap());
    assert_eq!(st.smembers(b"s").unwrap().len(), n);
}

/// The arc's headline claim, hash door: a write under a pinned view
/// clones the routed bucket only.
#[test]
fn cow_write_under_pinned_view_clones_one_bucket() {
    let mut st = Store::new();
    hset_n(&mut st, b"h", 3 * HS_PROMOTE);
    let view = st.collect_snapshot();
    st.hset(b"h", &[(b"after-pin".as_slice(), b"x".as_slice())]).unwrap();

    let Some(Value::SegHash(live)) = st.map.get(b"h".as_slice()).map(|e| &e.value) else {
        panic!("still sharded");
    };
    let stats = live.bucket_stats();
    let unique = stats.iter().filter(|(rc, _)| *rc == 1).count();
    let shared = stats.iter().filter(|(rc, _)| *rc >= 2).count();
    assert_eq!(unique, 1, "exactly the routed bucket is unshared");
    assert!(shared >= 2, "untouched buckets stay view-shared");

    // The view still serves the pre-write field count.
    let mut view_len = 0usize;
    view.each(|_, v, _| {
        if let Value::SegHash(h) = v {
            view_len = h.len();
        }
    });
    assert_eq!(view_len, 3 * HS_PROMOTE);
    assert_eq!(st.hlen(b"h").unwrap(), 3 * HS_PROMOTE + 1);
}

/// hdel / hincrby semantics on the sharded encoding.
#[test]
fn sharded_hash_ops_match_semantics() {
    let mut st = Store::new();
    let n = HS_PROMOTE + 100;
    hset_n(&mut st, b"h", n);
    assert!(is_seghash(&st, b"h"));

    // Overwrite keeps count; new field bumps it.
    st.hset(b"h", &[(b"field-00000007".as_slice(), b"rewritten".as_slice())]).unwrap();
    assert_eq!(st.hlen(b"h").unwrap(), n);
    assert_eq!(st.hget(b"h", b"field-00000007").unwrap().unwrap(), b"rewritten");

    // HDEL removes across buckets.
    let removed = st
        .hdel(
            b"h",
            &[b"field-00000001".as_slice(), b"field-00000002".as_slice(), b"nope".as_slice()],
        )
        .unwrap();
    assert_eq!(removed, 2);
    assert_eq!(st.hlen(b"h").unwrap(), n - 2);
    assert!(st.hget(b"h", b"field-00000001").unwrap().is_none());

    // HINCRBY through the encoding-blind facade.
    st.hset(b"h", &[(b"ctr".as_slice(), b"41".as_slice())]).unwrap();
    assert_eq!(st.hincrby(b"h", b"ctr", 1).unwrap(), 42);
    assert_eq!(st.hget(b"h", b"ctr").unwrap().unwrap(), b"42");
    assert!((st.hincrbyfloat(b"h", b"fctr", 0.5).unwrap() - 0.5).abs() < 1e-9);
}

/// An HINCRBY-only workload crosses the promotion boundary too (the
/// hash_mut path promotes, not just hset_one).
#[test]
fn hincr_only_workload_promotes() {
    let mut st = Store::new();
    for i in 0..HS_PROMOTE + 2 {
        let f = alloc::format!("c{i:08}");
        st.hincrby(b"h", f.as_bytes(), 1).unwrap();
    }
    assert!(is_seghash(&st, b"h"));
    assert_eq!(st.hlen(b"h").unwrap(), HS_PROMOTE + 2);
    assert_eq!(st.hincrby(b"h", b"c00000000", 41).unwrap(), 42);
}

/// srem / spop / srandmember on the sharded set.
#[test]
fn sharded_set_ops_match_semantics() {
    let mut st = Store::new();
    let n = HS_PROMOTE + 200;
    sadd_n(&mut st, b"s", n);

    let removed = st.srem(b"s", &[b"member-00000000".as_slice(), b"gone".as_slice()]).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(st.scard(b"s").unwrap(), n - 1);

    let popped = st.spop(b"s", 25).unwrap();
    assert_eq!(popped.len(), 25);
    assert_eq!(st.scard(b"s").unwrap(), n - 26);
    for m in &popped {
        assert!(!st.sismember(b"s", m).unwrap(), "popped member really removed");
    }

    let sample = st.srandmember(b"s", 10).unwrap();
    assert_eq!(sample.len(), 10);
    let unique: alloc::collections::BTreeSet<_> = sample.iter().collect();
    assert_eq!(unique.len(), 10, "positive-count SRANDMEMBER is distinct");
    for m in &sample {
        assert!(st.sismember(b"s", m).unwrap(), "sampled member still present");
    }

    let with_rep = st.srandmember_with_repeats(b"s", 5).unwrap();
    assert_eq!(with_rep.len(), 5);
}

/// Weight accounting survives the sharded-op zoo: DEL returns
/// `used_memory` to the pre-key baseline.
#[test]
fn accounting_round_trips_through_sharded_ops() {
    let mut st = Store::new();
    let baseline = st.used_memory();
    hset_n(&mut st, b"h", HS_PROMOTE + 300);
    st.hdel(b"h", &[b"field-00000004".as_slice()]).unwrap();
    st.hincrby(b"h", b"x", 7).unwrap();
    sadd_n(&mut st, b"s", HS_PROMOTE + 100);
    st.srem(b"s", &[b"member-00000003".as_slice()]).unwrap();
    st.spop(b"s", 10).unwrap();
    assert!(st.used_memory() > baseline);
    assert_eq!(st.del(&[b"h".as_slice(), b"s".as_slice()]), 2);
    assert_eq!(st.used_memory(), baseline);
}

/// Loads apply the same encoding switch.
#[test]
fn loads_apply_the_encoding_switch() {
    let mut st = Store::new();
    let big: alloc::vec::Vec<(alloc::vec::Vec<u8>, alloc::vec::Vec<u8>)> =
        (0..HS_PROMOTE + 1).map(|i| (alloc::format!("f{i}").into_bytes(), b"v".to_vec())).collect();
    st.load_hash(b"h".to_vec(), big, None);
    assert!(is_seghash(&st, b"h"));
    assert_eq!(st.hlen(b"h").unwrap(), HS_PROMOTE + 1);

    let members: alloc::vec::Vec<alloc::vec::Vec<u8>> =
        (0..HS_PROMOTE + 1).map(|i| alloc::format!("m{i}").into_bytes()).collect();
    st.load_set(b"s".to_vec(), members, None);
    assert!(is_segset(&st, b"s"));
    assert_eq!(st.scard(b"s").unwrap(), HS_PROMOTE + 1);

    let small: alloc::vec::Vec<(alloc::vec::Vec<u8>, alloc::vec::Vec<u8>)> =
        (0..100).map(|i| (alloc::format!("f{i}").into_bytes(), b"v".to_vec())).collect();
    st.load_hash(b"hs".to_vec(), small, None);
    assert!(!is_seghash(&st, b"hs"));
}
