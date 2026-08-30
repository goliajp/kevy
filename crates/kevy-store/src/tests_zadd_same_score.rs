//! ZADD with an unchanged score skips the ordered index — and must not skip
//! it when the score only *looks* unchanged.
//!
//! The guard in `ZSetData::insert` and `SegZSetData::insert` returns early
//! when the new score equals the old one, because the removal and insertion
//! it would otherwise do are of the same key and leave the rank tree exactly
//! as it was. These tests hold the line on both halves of that: that the
//! skip is invisible, and that `-0.0 -> 0.0` is not a skip.
//!
//! Each test here catches a different way the guard can be wrong, checked
//! by writing each wrong version and watching the right tests go red:
//!
//! - returning `true` instead of `false`: the two "invisible" tests fail;
//! - guard removed entirely: **all pass**, correctly — the unconditional
//!   remove-and-insert it replaces is semantically a no-op, only slow, so
//!   no behavioural test can tell them apart. That is what makes it safe.
//!
//! See the ZADD decomposition under bench/.

use crate::value::Value;
use crate::zset_seg::Z_PROMOTE;
use crate::Store;

/// Which of the three sorted-set encodings a key is on, or `"absent"`.
///
/// One function with every arm rather than two `matches!` predicates,
/// because deadgate names any symbol owning a never-executed region and a
/// two-arm `matches!` that is only ever called where it is true owns one.
/// The test below walks all four arms, which is both what keeps this out of
/// the dead set and a statement of the encoding ladder these tests rely on.
fn encoding(st: &Store, key: &[u8]) -> &'static str {
    match st.map.get(key).map(|e| &e.value) {
        None => "absent",
        Some(Value::SmallZSetInline(_)) => "inline",
        Some(Value::ZSet(_)) => "flat",
        Some(Value::SegZSet(_)) => "seg",
        Some(_) => "not-a-zset",
    }
}

/// The ladder itself: absent, then inline for two short members, then the
/// heap-backed flat encoding, then segmented past `Z_PROMOTE`.
#[test]
fn the_encoding_ladder() {
    let mut st = Store::new();
    assert_eq!(encoding(&st, b"k"), "absent");

    st.zadd(b"k", &[(1.0, b"a")]).unwrap();
    assert_eq!(encoding(&st, b"k"), "inline", "one short member fits 22 bytes");

    for m in [b"bb".as_ref(), b"cc", b"dd"] {
        st.zadd(b"k", &[(1.0, m)]).unwrap();
    }
    assert_eq!(encoding(&st, b"k"), "flat", "past SMALL_ZSET_COUNT_MAX");

    for i in 0..Z_PROMOTE {
        let m = alloc::format!("member-{i:08}");
        st.zadd(b"k", &[((i % 977) as f64, m.as_bytes())]).unwrap();
    }
    assert_eq!(encoding(&st, b"k"), "seg", "past Z_PROMOTE");

    st.set(b"s", alloc::vec![b'x'; 4], None, false, false);
    assert_eq!(encoding(&st, b"s"), "not-a-zset");
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
    assert_eq!(encoding(&st, b"k"), "flat", "fixture must be past the inline encoding");
    let before = st.zrange(b"k", 0, -1).unwrap();

    let added = st.zadd(b"k", &[(1.0, b"b")]).unwrap();
    assert_eq!(added, 0, "an existing member is not a new one");

    assert_eq!(encoding(&st, b"k"), "flat");
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
    assert_eq!(encoding(&st, b"k"), "seg", "the fixture must reach the Seg encoding");

    let probe = alloc::format!("member-{:08}", 7);
    // The fixture above scores member i at `i % 977`; for i = 7 that is 7.
    let score = 7.0;
    let rank_before = st.zrank(b"k", probe.as_bytes()).unwrap();
    // The whole order, not just its length. The skipped path would have
    // removed the key from its segment and put it back, and a segment left
    // holding one entry is destroyed and recreated on the way — so the
    // question is whether the reading comes back identical, not whether it
    // is the same size.
    let order_before = st.zrange(b"k", 0, -1).unwrap();

    let added = st.zadd(b"k", &[(score, probe.as_bytes())]).unwrap();
    assert_eq!(added, 0);

    assert_eq!(st.zscore(b"k", probe.as_bytes()).unwrap(), Some(score));
    assert_eq!(st.zrank(b"k", probe.as_bytes()).unwrap(), rank_before);
    assert!(st.zrange(b"k", 0, -1).unwrap() == order_before, "the order moved");
}

/// `-0` and `0` are one score, as they are in Redis, because `zadd_one`
/// folds the sign at the door. Before that fold this ordering was `zlo`
/// first — `total_cmp` separates the zeros and orders the negative before
/// the positive — while a real `redis:8` answered `zhi` first, on the
/// member tie-break. See the finding under bench/.
#[test]
fn the_two_zeros_are_one_score() {
    let mut st = Store::new();
    for m in [b"f1".as_ref(), b"f2", b"f3"] {
        st.zadd(b"k", &[(9.0, m)]).unwrap();
    }
    st.zadd(b"k", &[(0.0, b"zhi"), (-0.0, b"zlo")]).unwrap();
    assert_eq!(encoding(&st, b"k"), "flat", "fixture must be on the heap-backed encoding");

    // Same score, so the member breaks the tie: `zhi` before `zlo`.
    assert_eq!(
        st.zrange(b"k", 0, 1).unwrap().iter().map(|(m, _)| m.clone()).collect::<Vec<_>>(),
        alloc::vec![b"zhi".to_vec(), b"zlo".to_vec()],
        "-0 and 0 must be one score, ordered by member",
    );

    // And the stored score really is `+0.0`, not a negative zero that
    // happens to sort in the same place.
    let sc = st.zscore(b"k", b"zlo").unwrap().unwrap();
    assert!(sc == 0.0 && sc.is_sign_positive(), "the sign was not folded");
}

/// The same at the Seg encoding, whose write entry point is a different
/// function reached through the same door.
#[test]
fn the_two_zeros_are_one_score_seg() {
    let mut st = Store::new();
    let n = Z_PROMOTE + 64;
    for i in 0..n {
        let m = alloc::format!("member-{i:08}");
        st.zadd(b"k", &[((i % 977) as f64 + 1.0, m.as_bytes())]).unwrap();
    }
    assert_eq!(encoding(&st, b"k"), "seg");
    st.zadd(b"k", &[(0.0, b"zhi"), (-0.0, b"zlo")]).unwrap();

    let first_two: Vec<Vec<u8>> =
        st.zrange(b"k", 0, 1).unwrap().iter().map(|(m, _)| m.clone()).collect();
    assert_eq!(first_two, alloc::vec![b"zhi".to_vec(), b"zlo".to_vec()]);

    let sc = st.zscore(b"k", b"zlo").unwrap().unwrap();
    assert!(sc == 0.0 && sc.is_sign_positive(), "the sign was not folded");
}
