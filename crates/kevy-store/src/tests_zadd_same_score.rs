//! ZADD with an unchanged score skips the ordered index — and must not skip
//! it when the score only *looks* unchanged.
//!
//! The guard in `ZSetData::insert` and `SegZSetData::insert` returns early
//! when the new score equals the old one, because the removal and insertion
//! it would otherwise do are of the same key and leave the rank tree exactly
//! as it was. These tests hold the line on both halves of that: that the
//! skip is invisible, and that `-0.0 -> 0.0` is not a skip.
//!
//! See bench/PERF-DECOMP-2026-08-30-zadd-arena-cell.md.

use crate::value::Value;
use crate::zset_seg::Z_PROMOTE;
use crate::Store;

fn is_seg(st: &Store, key: &[u8]) -> bool {
    matches!(st.map.get(key).map(|e| &e.value), Some(Value::SegZSet(_)))
}

/// The heap-backed `ZSetData` — the one with `by_member` and `by_score` —
/// as opposed to `SmallZSetInline`, which holds up to two short members
/// packed in 22 bytes and has no ordered index to corrupt.
///
/// Every test here asserts its encoding. The first version of the -0.0 test
/// below used two two-byte members, which fit inline, so it exercised
/// `SmallZSetData::try_set` and passed against a deliberately broken guard
/// in `ZSetData::insert`. A test that names a path has to prove it is on it.
fn is_flat(st: &Store, key: &[u8]) -> bool {
    matches!(st.map.get(key).map(|e| &e.value), Some(Value::ZSet(_)))
}

/// Re-adding at the same score changes nothing observable, on the Flat
/// encoding: not the score, not the rank, not the reported count of new
/// members, not the order of its neighbours.
#[test]
fn same_score_readd_is_invisible_flat() {
    let mut st = Store::new();
    for (i, m) in [b"a".as_ref(), b"b", b"c", b"d", b"e"].iter().enumerate() {
        st.zadd(b"k", &[(i as f64, m)]).unwrap();
    }
    assert!(is_flat(&st, b"k"), "fixture must be past the inline encoding");
    let before = st.zrange(b"k", 0, -1).unwrap();

    let added = st.zadd(b"k", &[(1.0, b"b")]).unwrap();
    assert_eq!(added, 0, "an existing member is not a new one");

    assert!(is_flat(&st, b"k"));
    assert_eq!(st.zscore(b"k", b"b").unwrap(), Some(1.0));
    assert_eq!(st.zrank(b"k", b"b").unwrap(), Some(1));
    assert_eq!(st.zrange(b"k", 0, -1).unwrap(), before, "order moved");
}

/// The same, past `Z_PROMOTE`, where the value is a `SegZSet` and the skipped
/// path would have gone through `Arc::make_mut` on a segment.
#[test]
fn same_score_readd_is_invisible_seg() {
    let mut st = Store::new();
    let n = Z_PROMOTE + 64;
    for i in 0..n {
        let m = alloc::format!("member-{i:08}");
        st.zadd(b"k", &[((i % 977) as f64, m.as_bytes())]).unwrap();
    }
    assert!(is_seg(&st, b"k"), "the fixture must reach the Seg encoding");

    let probe = alloc::format!("member-{:08}", 7);
    let score = (7 % 977) as f64;
    let rank_before = st.zrank(b"k", probe.as_bytes()).unwrap();
    let len_before = st.zrange(b"k", 0, -1).unwrap().len();

    let added = st.zadd(b"k", &[(score, probe.as_bytes())]).unwrap();
    assert_eq!(added, 0);

    assert_eq!(st.zscore(b"k", probe.as_bytes()).unwrap(), Some(score));
    assert_eq!(st.zrank(b"k", probe.as_bytes()).unwrap(), rank_before);
    assert_eq!(st.zrange(b"k", 0, -1).unwrap().len(), len_before);
}

/// `-0.0` and `0.0` are equal under `f64`'s `==` and distinct under the
/// order the rank tree keys on. A guard written with `==` would treat this
/// update as a no-op and leave the member indexed at `-0.0` while its score
/// reads `0.0` — the two halves of one sorted set disagreeing.
///
/// `ZADD z -0 m` is accepted by a live server, so this is reachable.
#[test]
fn negative_zero_to_positive_zero_is_a_real_update() {
    let mut st = Store::new();
    // Three filler members first: two short ones would fit `SmallZSetInline`,
    // which has no ordered index and would not reach the guard at all.
    for m in [b"f1".as_ref(), b"f2", b"f3"] {
        st.zadd(b"k", &[(9.0, m)]).unwrap();
    }
    // `lo` sits at -0.0 and `hi` at 0.0, so the pair orders lo, hi.
    st.zadd(b"k", &[(-0.0, b"lo"), (0.0, b"hi")]).unwrap();
    assert!(is_flat(&st, b"k"), "fixture must be on the heap-backed encoding");
    assert_eq!(
        st.zrange(b"k", 0, 1).unwrap().iter().map(|(m, _)| m.clone()).collect::<Vec<_>>(),
        alloc::vec![b"lo".to_vec(), b"hi".to_vec()],
        "-0.0 must sort before 0.0",
    );

    // Move `lo` from -0.0 to 0.0. Ties break on the member bytes, so it
    // should now sort AFTER `hi`.
    st.zadd(b"k", &[(0.0, b"lo")]).unwrap();
    assert_eq!(
        st.zrange(b"k", 0, 1).unwrap().iter().map(|(m, _)| m.clone()).collect::<Vec<_>>(),
        alloc::vec![b"hi".to_vec(), b"lo".to_vec()],
        "the -0.0 -> 0.0 update was skipped: the index still holds -0.0",
    );
}

/// The same trap on the Seg encoding.
#[test]
fn negative_zero_to_positive_zero_is_a_real_update_seg() {
    let mut st = Store::new();
    let n = Z_PROMOTE + 64;
    for i in 0..n {
        let m = alloc::format!("member-{i:08}");
        st.zadd(b"k", &[((i % 977) as f64 + 1.0, m.as_bytes())]).unwrap();
    }
    assert!(is_seg(&st, b"k"));
    // Two members at the bottom of the order, one at -0.0 and one at 0.0.
    st.zadd(b"k", &[(-0.0, b"zlo"), (0.0, b"zhi")]).unwrap();
    let first_two: Vec<Vec<u8>> =
        st.zrange(b"k", 0, 1).unwrap().iter().map(|(m, _)| m.clone()).collect();
    assert_eq!(first_two, alloc::vec![b"zlo".to_vec(), b"zhi".to_vec()]);

    st.zadd(b"k", &[(0.0, b"zlo")]).unwrap();
    let first_two: Vec<Vec<u8>> =
        st.zrange(b"k", 0, 1).unwrap().iter().map(|(m, _)| m.clone()).collect();
    assert_eq!(
        first_two,
        alloc::vec![b"zhi".to_vec(), b"zlo".to_vec()],
        "the -0.0 -> 0.0 update was skipped on the Seg path",
    );
}
