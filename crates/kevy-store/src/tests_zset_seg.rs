//! SegZSet (segmented sorted set COW) tests: promotion, ordered-segment
//! invariants at scale, per-segment COW behavior under a pinned view,
//! semantics vs a model, and accounting round-trips.

use crate::value::Value;
use crate::zset_seg::Z_PROMOTE;
use crate::Store;

fn zadd_n(st: &mut Store, key: &[u8], n: usize) {
    // Scores interleave (i % 977 major, i minor via member tiebreak) so
    // ordered ranks differ from insertion order and exercise routing.
    for i in 0..n {
        let m = alloc::format!("member-{i:08}");
        let score = (i % 977) as f64;
        st.zadd(key, &[(score, m.as_bytes())]).unwrap();
    }
}

fn is_segzset(st: &Store, key: &[u8]) -> bool {
    matches!(st.map.get(key).map(|e| &e.value), Some(Value::SegZSet(_)))
}

/// Model: sorted (score, member) pairs for comparison.
fn model(n: usize) -> alloc::vec::Vec<(alloc::vec::Vec<u8>, f64)> {
    let mut v: alloc::vec::Vec<(alloc::vec::Vec<u8>, f64)> = (0..n)
        .map(|i| (alloc::format!("member-{i:08}").into_bytes(), (i % 977) as f64))
        .collect();
    v.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    v
}

#[test]
#[cfg_attr(miri, ignore = "16K-element promotion loops; hours under miri's interpretation overhead. The unsafe surface these paths reach (kevy-map slot management) is miri-covered by the flat-zset subset and kevy-map's own suite; SegZSet itself is safe composition (kevy-store is forbid(unsafe_code)).")]
fn zadd_promotes_and_order_holds_across_segments() {
    let mut st = Store::new();
    let n = 2 * Z_PROMOTE + 100;
    zadd_n(&mut st, b"z", n);
    assert!(is_segzset(&st, b"z"));
    assert_eq!(st.zcard(b"z").unwrap(), n);

    let want = model(n);
    // Full ZRANGE agrees with the model.
    let got = st.zrange(b"z", 0, -1).unwrap();
    assert_eq!(got.len(), n);
    for (g, w) in got.iter().zip(want.iter()) {
        assert_eq!((&g.0, g.1), (&w.0, w.1));
    }
    // A window deep in the middle (ordered_from seek across segments).
    let mid = st.zrange(b"z", (n / 2) as i64, (n / 2 + 5) as i64).unwrap();
    assert_eq!(mid.len(), 6);
    assert_eq!(mid[0].0, want[n / 2].0);

    // ZSCORE + ZRANK spot checks.
    for i in (0..n).step_by(1201) {
        let m = alloc::format!("member-{i:08}");
        assert_eq!(st.zscore(b"z", m.as_bytes()).unwrap(), Some((i % 977) as f64));
        let rank = st.zrank(b"z", m.as_bytes()).unwrap().expect("present");
        assert_eq!(want[rank].0, m.as_bytes());
    }
}

#[test]
#[cfg_attr(miri, ignore = "16K-element promotion loops; hours under miri's interpretation overhead. The unsafe surface these paths reach (kevy-map slot management) is miri-covered by the flat-zset subset and kevy-map's own suite; SegZSet itself is safe composition (kevy-store is forbid(unsafe_code)).")]
fn zincr_only_workload_promotes() {
    let mut st = Store::new();
    for i in 0..Z_PROMOTE + 2 {
        let m = alloc::format!("m{i:08}");
        st.zincrby(b"z", 1.5, m.as_bytes()).unwrap();
    }
    assert!(is_segzset(&st, b"z"));
    assert_eq!(st.zcard(b"z").unwrap(), Z_PROMOTE + 2);
    assert_eq!(st.zincrby(b"z", 2.0, b"m00000000").unwrap(), 3.5);
}

/// The arc's headline claim, zset door: a write under a pinned view
/// clones one member bucket + one segment tree.
#[test]
#[cfg_attr(miri, ignore = "16K-element promotion loops; hours under miri's interpretation overhead. The unsafe surface these paths reach (kevy-map slot management) is miri-covered by the flat-zset subset and kevy-map's own suite; SegZSet itself is safe composition (kevy-store is forbid(unsafe_code)).")]
fn cow_write_under_pinned_view_clones_one_segment() {
    let mut st = Store::new();
    let n = 3 * Z_PROMOTE;
    zadd_n(&mut st, b"z", n);
    let view = st.collect_snapshot();
    st.zadd(b"z", &[(50.5, b"after-pin".as_slice())]).unwrap();

    let Some(Value::SegZSet(live)) = st.map.get(b"z".as_slice()).map(|e| &e.value) else {
        panic!("still segmented");
    };
    let stats = live.seg_stats();
    let unique = stats.iter().filter(|(rc, _)| *rc == 1).count();
    let shared = stats.iter().filter(|(rc, _)| *rc >= 2).count();
    assert_eq!(unique, 1, "exactly the routed segment tree is unshared");
    assert!(shared >= 2, "untouched segment trees stay view-shared");

    let mut view_len = 0usize;
    view.each(|_, v, _| {
        if let Value::SegZSet(z) = v {
            view_len = z.len();
        }
    });
    assert_eq!(view_len, n);
    assert_eq!(st.zcard(b"z").unwrap(), n + 1);
}

#[test]
#[cfg_attr(miri, ignore = "16K-element promotion loops; hours under miri's interpretation overhead. The unsafe surface these paths reach (kevy-map slot management) is miri-covered by the flat-zset subset and kevy-map's own suite; SegZSet itself is safe composition (kevy-store is forbid(unsafe_code)).")]
fn segmented_ops_match_semantics() {
    let mut st = Store::new();
    let n = Z_PROMOTE + 500;
    zadd_n(&mut st, b"z", n);
    assert!(is_segzset(&st, b"z"));

    // Score update (not an add) moves the member's ordered position.
    st.zadd(b"z", &[(976.5, b"member-00000000".as_slice())]).unwrap();
    assert_eq!(st.zcard(b"z").unwrap(), n);
    assert_eq!(st.zscore(b"z", b"member-00000000").unwrap(), Some(976.5));
    let r = st.zrank(b"z", b"member-00000000").unwrap().unwrap();
    assert!(r > n - 200, "bumped to the top score region (rank {r})");

    // ZCOUNT / ZRANGEBYSCORE against a countable predicate.
    let min = crate::value::ScoreBound { value: 100.0, exclusive: false };
    let max = crate::value::ScoreBound { value: 105.0, exclusive: false };
    let counted = st.zcount(b"z", min, max).unwrap();
    let ranged = st
        .zrange_by_score(
            b"z",
            crate::value::ScoreBound { value: 100.0, exclusive: false },
            crate::value::ScoreBound { value: 105.0, exclusive: false },
        )
        .unwrap();
    assert_eq!(counted, ranged.len());
    assert!(ranged.iter().all(|(_, sc)| (100.0..=105.0).contains(sc)));

    // ZPOPMIN pops ascending; ZREM removes across segments.
    let popped = st.zpopmin(b"z", 3).unwrap();
    assert_eq!(popped.len(), 3);
    assert!(popped.windows(2).all(|w| w[0].1 <= w[1].1));
    let removed = st
        .zrem(b"z", &[b"member-00001000".as_slice(), b"absent".as_slice()])
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(st.zcard(b"z").unwrap(), n - 4);

    // ZREMRANGEBYRANK drops an exact window.
    let before = st.zcard(b"z").unwrap();
    let dropped = st.zrem_range_by_rank(b"z", 10, 19).unwrap();
    assert_eq!(dropped, 10);
    assert_eq!(st.zcard(b"z").unwrap(), before - 10);
}

#[test]
#[cfg_attr(miri, ignore = "16K-element promotion loops; hours under miri's interpretation overhead. The unsafe surface these paths reach (kevy-map slot management) is miri-covered by the flat-zset subset and kevy-map's own suite; SegZSet itself is safe composition (kevy-store is forbid(unsafe_code)).")]
fn accounting_round_trips_through_segmented_ops() {
    let mut st = Store::new();
    let baseline = st.used_memory();
    zadd_n(&mut st, b"z", Z_PROMOTE + 300);
    st.zincrby(b"z", 5.0, b"member-00000007").unwrap();
    st.zrem(b"z", &[b"member-00000009".as_slice()]).unwrap();
    st.zpopmin(b"z", 20).unwrap();
    st.zrem_range_by_rank(b"z", 0, 9).unwrap();
    assert!(st.used_memory() > baseline);
    assert_eq!(st.del(&[b"z".as_slice()]), 1);
    assert_eq!(st.used_memory(), baseline);
}

#[test]
#[cfg_attr(miri, ignore = "16K-element promotion loops; hours under miri's interpretation overhead. The unsafe surface these paths reach (kevy-map slot management) is miri-covered by the flat-zset subset and kevy-map's own suite; SegZSet itself is safe composition (kevy-store is forbid(unsafe_code)).")]
fn load_zset_applies_the_encoding_switch() {
    let mut st = Store::new();
    let big: alloc::vec::Vec<(alloc::vec::Vec<u8>, f64)> = (0..Z_PROMOTE + 1)
        .map(|i| (alloc::format!("m{i}").into_bytes(), i as f64))
        .collect();
    st.load_zset(b"big".to_vec(), big, None);
    assert!(is_segzset(&st, b"big"));
    assert_eq!(st.zcard(b"big").unwrap(), Z_PROMOTE + 1);
    assert_eq!(st.zscore(b"big", b"m17").unwrap(), Some(17.0));

    let small: alloc::vec::Vec<(alloc::vec::Vec<u8>, f64)> =
        (0..100).map(|i| (alloc::format!("m{i}").into_bytes(), i as f64)).collect();
    st.load_zset(b"small".to_vec(), small, None);
    assert!(!is_segzset(&st, b"small"));
}
