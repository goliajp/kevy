use super::*;

fn seg() -> AggSegment {
    let mut s = AggSegment::new();
    // orders: group = status, value = amount
    for (k, g, v) in [
        ("o1", "paid", 100),
        ("o2", "paid", 250),
        ("o3", "open", 40),
        ("o4", "paid", 100),
        ("o5", "open", 999),
    ] {
        s.apply(
            k.as_bytes(),
            AggRow::Member { group: g.as_bytes().to_vec(), value: IndexValue::I64(v) },
        );
    }
    s
}

#[test]
fn group_stats_exact() {
    let s = seg();
    let g = s.group(b"paid");
    assert_eq!((g.count, g.sum), (3, 450.0));
    assert_eq!(g.min, Some(IndexValue::I64(100)));
    assert_eq!(g.max, Some(IndexValue::I64(250)));
    assert_eq!(g.avg(), Some(150.0));
    let none = s.group(b"nope");
    assert_eq!(none.count, 0);
    assert!(none.min.is_none() && none.avg().is_none());
}

#[test]
fn min_max_exact_under_delete_and_update() {
    let mut s = seg();
    // delete the paid max (o2=250): max must fall back to 100
    s.apply(b"o2", AggRow::Removed);
    let g = s.group(b"paid");
    assert_eq!((g.count, g.max.clone()), (2, Some(IndexValue::I64(100))));
    // duplicate values: removing ONE 100 keeps the other
    s.apply(b"o1", AggRow::Removed);
    let g = s.group(b"paid");
    assert_eq!((g.count, g.min.clone()), (1, Some(IndexValue::I64(100))));
    // update moves a row across groups
    s.apply(b"o3", AggRow::Member { group: b"paid".to_vec(), value: IndexValue::I64(40) });
    assert_eq!(s.group(b"paid").count, 2);
    assert_eq!(s.group(b"open").count, 1);
    assert_eq!(s.group(b"paid").min, Some(IndexValue::I64(40)));
    // last row leaving a group drops the group
    s.apply(b"o5", AggRow::Removed);
    assert_eq!(s.group(b"open").count, 0);
    assert_eq!(s.stats().groups, 1);
}

#[test]
fn top_groups_all_metrics() {
    let s = seg();
    let top = s.top_groups(AggBy::Count, 10);
    assert_eq!(top[0].0, b"paid".to_vec());
    let top = s.top_groups(AggBy::Sum, 10);
    assert_eq!(top[0].0, b"open".to_vec(), "open sum 1039 > paid 450");
    let top = s.top_groups(AggBy::Min, 10);
    assert_eq!(top[0].0, b"open".to_vec(), "min ascending: 40 first");
    let top = s.top_groups(AggBy::Max, 1);
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].0, b"open".to_vec(), "max 999");
}

#[test]
fn excluded_counted_and_merge() {
    let mut s = seg();
    s.apply(b"bad1", AggRow::Excluded);
    s.apply(b"bad2", AggRow::Excluded);
    assert_eq!(s.stats().excluded, 2);
    assert!(s.contains(b"o1") && !s.contains(b"bad1"));
    // cross-shard merge semantics
    let mut a = s.group(b"paid");
    let b = seg().group(b"paid");
    GroupStats::merge(&mut a, &b);
    assert_eq!((a.count, a.sum), (6, 900.0));
    assert_eq!(a.min, Some(IndexValue::I64(100)));
    assert_eq!(a.max, Some(IndexValue::I64(250)));
    // merge with an empty partial keeps extremes
    let mut e = GroupStats { count: 0, sum: 0.0, min: None, max: None };
    GroupStats::merge(&mut e, &a);
    assert_eq!(e.max, Some(IndexValue::I64(250)));
}

#[test]
fn stats_bytes_nonzero() {
    let s = seg();
    let st = s.stats();
    assert_eq!((st.groups, st.rows), (2, 5));
    assert!(st.approx_bytes > 0);
}

/// `stats()` reads running counters instead of walking the
/// maps. A mixed workload — inserts, the same-group fast path,
/// group moves, removals down to empty — holds them to the walking
/// reference after every step.
#[test]
fn running_stats_never_drift_from_the_walking_reference() {
    let mut s = AggSegment::new();
    let check = |s: &AggSegment, at: &str| {
        assert_eq!(s.stats(), s.recompute_stats(), "counter drift after {at}");
    };
    let mut x = 0x2545F491u64;
    let mut next = move || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (x >> 33) as u32
    };
    let groups = [b"eng".as_slice(), b"sales", b"ops"];
    for round in 0..300u32 {
        let key = format!("r:{}", next() % 30);
        match next() % 6 {
            0 => s.apply(key.as_bytes(), AggRow::Removed),
            1 => s.apply(key.as_bytes(), AggRow::Excluded), // excluded
            _ => {
                let g = groups[(next() % 3) as usize].to_vec();
                // Small value domain forces shared distinct entries.
                let v = IndexValue::I64(i64::from(next() % 7));
                s.apply(key.as_bytes(), AggRow::Member { group: g, value: v });
            }
        }
        check(&s, &format!("round {round}"));
    }
    for i in 0..30u32 {
        s.apply(format!("r:{i}").as_bytes(), AggRow::Removed);
    }
    check(&s, "full drain");
    let end = s.stats();
    assert_eq!((end.groups, end.rows), (0, 0));
}
