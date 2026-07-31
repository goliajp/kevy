//! Selection-semantics tests for the clause-carrying scalar query —
//! each clause's contract restated as an assertion, mirroring the text
//! surface's behaviour exactly.

use super::*;
use crate::catalog::ValType;
use crate::value::ValueTest;
use crate::segment::Segment;
use crate::value::IndexValue;

fn i(v: i64) -> IndexValue {
    IndexValue::I64(v)
}

/// A one-VALUES-field segment: rows u1..u5 with ages 10..50 and a
/// stored `city` (u3 has NO stored value; u5's does not coerce as i64
/// when the tests declare it numeric).
fn seeded() -> Segment {
    let mut s = Segment::with_values(1);
    s.apply_with_values(b"u1", Some(i(10)), &[Some(b"tokyo")]);
    s.apply_with_values(b"u2", Some(i(20)), &[Some(b"osaka")]);
    s.apply_with_values(b"u3", Some(i(30)), &[None]);
    s.apply_with_values(b"u4", Some(i(40)), &[Some(b"tokyo")]);
    s.apply_with_values(b"u5", Some(i(50)), &[Some(b"kyoto")]);
    s
}

fn clauses<'a>() -> ScalarClauses<'a> {
    ScalarClauses { filters: &[], sort: None, distinct: None, facets: &[], fetch: 100 }
}

fn keys(hits: &[ScalarHit]) -> Vec<&[u8]> {
    hits.iter().map(|h| h.key.as_slice()).collect()
}

#[test]
fn values_store_update_delete_follow_the_entry() {
    let mut s = Segment::with_values(1);
    s.apply_with_values(b"u1", Some(i(1)), &[Some(b"a")]);
    assert_eq!(s.stored(b"u1", 0), Some(&b"a"[..]));
    // update replaces
    s.apply_with_values(b"u1", Some(i(2)), &[Some(b"b")]);
    assert_eq!(s.stored(b"u1", 0), Some(&b"b"[..]));
    // coerce-failure excludes the row AND drops its values
    s.apply_with_values(b"u1", None, &[Some(b"c")]);
    assert_eq!(s.stored(b"u1", 0), None);
    // re-add then remove drops them too
    s.apply_with_values(b"u1", Some(i(3)), &[Some(b"d")]);
    s.remove(b"u1");
    assert_eq!(s.stored(b"u1", 0), None);
    // a segment without the declaration answers None and stays byte-free
    let plain = Segment::new();
    assert_eq!(plain.stored(b"u1", 0), None);
}

#[test]
fn values_heap_joins_the_memory_term_only_when_declared() {
    let mut with = Segment::with_values(1);
    with.apply_with_values(b"u1", Some(i(1)), &[Some(&[b'x'; 100])]);
    let mut without = Segment::new();
    without.apply(b"u1", Some(i(1)));
    assert!(with.stats().approx_bytes > without.stats().approx_bytes);
}

#[test]
fn filter_thins_the_driving_order_and_missing_fails() {
    let s = seeded();
    let t = ValueTest::eq(ValType::Str, b"tokyo").unwrap();
    let filters = [(0usize, t)];
    let c = ScalarClauses { filters: &filters, ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    // u3 has no stored value: absent is not a value, so it FAILS.
    assert_eq!(keys(&page.hits), vec![&b"u1"[..], b"u4"]);
    assert!(page.cursor.is_none(), "page not full → exhausted");
}

#[test]
fn filter_uncoercible_stored_value_is_excluded_not_matched() {
    let mut s = Segment::with_values(1);
    s.apply_with_values(b"u1", Some(i(1)), &[Some(b"12")]);
    s.apply_with_values(b"u2", Some(i(2)), &[Some(b"not-a-number")]);
    let t = ValueTest::range(ValType::I64, b"0", b"100").unwrap();
    let filters = [(0usize, t)];
    let c = ScalarClauses { filters: &filters, ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    assert_eq!(keys(&page.hits), vec![&b"u1"[..]], "text in a numeric range is not inside it");
}

#[test]
fn filter_pages_with_a_cursor_in_driving_order() {
    let s = seeded();
    let t = ValueTest::range(ValType::Str, b"a", b"zz").unwrap();
    let filters = [(0usize, t.clone())];
    let c = ScalarClauses { filters: &filters, fetch: 2, ..clauses() };
    let p1 = s.query_claused(&i(0), &i(100), None, &c);
    assert_eq!(keys(&p1.hits), vec![&b"u1"[..], b"u2"]);
    let cur = p1.cursor.expect("full page carries a cursor");
    let p2 = s.query_claused(&i(0), &i(100), Some(&cur), &c);
    // u3 skipped (no value), u4 + u5 close the range.
    assert_eq!(keys(&p2.hits), vec![&b"u4"[..], b"u5"]);
}

#[test]
fn sort_orders_by_the_stored_key_missing_last_both_directions() {
    let s = seeded();
    let c = ScalarClauses { sort: Some((0, false, ValType::Str)), ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    // kyoto, osaka, tokyo(u1), tokyo(u4 — key tiebreak), then the
    // valueless u3 LAST.
    assert_eq!(keys(&page.hits), vec![&b"u5"[..], b"u2", b"u1", b"u4", b"u3"]);
    let c = ScalarClauses { sort: Some((0, true, ValType::Str)), ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    // Descending flips the valued rows; missing stays LAST.
    assert_eq!(keys(&page.hits), vec![&b"u1"[..], b"u4", b"u2", b"u5", b"u3"]);
}

#[test]
fn sort_key_is_numeric_under_a_numeric_declaration() {
    let mut s = Segment::with_values(1);
    s.apply_with_values(b"a", Some(i(1)), &[Some(b"9")]);
    s.apply_with_values(b"b", Some(i(2)), &[Some(b"10")]);
    let c = ScalarClauses { sort: Some((0, false, ValType::I64)), ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    assert_eq!(keys(&page.hits), vec![&b"a"[..], b"b"], "9 < 10 numerically");
    let c = ScalarClauses { sort: Some((0, false, ValType::Str)), ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    assert_eq!(keys(&page.hits), vec![&b"b"[..], b"a"], "\"10\" < \"9\" as text");
}

#[test]
fn distinct_collapses_during_selection_and_no_value_is_its_own_group() {
    let s = seeded();
    let c = ScalarClauses { distinct: Some((0, ValType::Str)), ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    // tokyo collapses to its first (driving-order) row u1; the
    // valueless u3 survives as its own group.
    assert_eq!(keys(&page.hits), vec![&b"u1"[..], b"u2", b"u3", b"u5"]);
}

#[test]
fn distinct_identity_is_the_coerced_value() {
    let mut s = Segment::with_values(1);
    s.apply_with_values(b"a", Some(i(1)), &[Some(b"1")]);
    s.apply_with_values(b"b", Some(i(2)), &[Some(b"1.0")]);
    let c = ScalarClauses { distinct: Some((0, ValType::F64)), ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    assert_eq!(keys(&page.hits), vec![&b"a"[..]], "1 and 1.0 are one f64 value");
}

#[test]
fn distinct_under_sort_keeps_the_best_group_representative() {
    let mut s = Segment::with_values(2);
    // field 0 = group, field 1 = sort key
    s.apply_with_values(b"a", Some(i(1)), &[Some(b"g1"), Some(b"5")]);
    s.apply_with_values(b"b", Some(i(2)), &[Some(b"g1"), Some(b"1")]);
    s.apply_with_values(b"c", Some(i(3)), &[Some(b"g2"), Some(b"3")]);
    let c = ScalarClauses {
        sort: Some((1, false, ValType::I64)),
        distinct: Some((0, ValType::Str)),
        ..clauses()
    };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    // g1's best ascending is b (1), then c (3); a (5) collapsed away.
    assert_eq!(keys(&page.hits), vec![&b"b"[..], b"c"]);
}

#[test]
fn facet_counts_the_whole_match_set_before_truncation() {
    let s = seeded();
    let facets = [(0usize, ValType::Str)];
    let c = ScalarClauses { facets: &facets, fetch: 1, ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    assert_eq!(keys(&page.hits), vec![&b"u1"[..]], "page truncated to fetch");
    // …but the counts cover every match: tokyo×2, kyoto, osaka (u3 has
    // no value — absence is not a bucket). Most frequent first, label
    // tiebreak.
    let labels: Vec<(&[u8], u64)> =
        page.facets[0].iter().map(|(_, l, n)| (l.as_slice(), *n)).collect();
    assert_eq!(labels, vec![(&b"tokyo"[..], 2), (b"kyoto", 1), (b"osaka", 1)]);
}

#[test]
fn filter_reduces_facet_counts_distinct_does_not() {
    let s = seeded();
    let t = ValueTest::eq(ValType::Str, b"tokyo").unwrap();
    let filters = [(0usize, t)];
    let facets = [(0usize, ValType::Str)];
    let c = ScalarClauses { filters: &filters, facets: &facets, ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    assert_eq!(page.facets[0], vec![(b"tokyo".to_vec(), b"tokyo".to_vec(), 2)]);

    let c = ScalarClauses {
        distinct: Some((0, ValType::Str)),
        facets: &facets,
        ..clauses()
    };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    assert_eq!(page.facets[0][0].2, 2, "DISTINCT collapses the page, not the counts");
    assert_eq!(keys(&page.hits).len(), 4, "page IS collapsed");
}

#[test]
fn selection_clauses_carry_no_cursor() {
    let s = seeded();
    let c = ScalarClauses { sort: Some((0, false, ValType::Str)), fetch: 2, ..clauses() };
    let page = s.query_claused(&i(0), &i(100), None, &c);
    assert_eq!(page.hits.len(), 2);
    assert!(page.cursor.is_none());
}

#[test]
fn merge_orders_collapses_offsets_and_cuts() {
    let hit = |k: &[u8], v: i64, okey: Option<&[u8]>, dkey: Option<&[u8]>| {
        (
            ScalarHit {
                key: k.to_vec(),
                value: i(v),
                okey: okey.map(<[u8]>::to_vec),
                dkey: dkey.map(<[u8]>::to_vec),
            },
            (),
        )
    };
    let keys = |hits: &[(ScalarHit, ())]| -> Vec<Vec<u8>> {
        hits.iter().map(|(h, ())| h.key.clone()).collect()
    };
    // Driving-order merge: k-way by (value, key).
    let merged = merge_claused(
        vec![hit(b"b", 2, None, None), hit(b"a", 1, None, None), hit(b"c", 1, None, None)],
        None,
        false,
        0,
        10,
    );
    assert_eq!(keys(&merged), [b"a".to_vec(), b"c".to_vec(), b"b".to_vec()]);
    // Sorted merge: by okey, missing last; then re-collapse; then the
    // offset drains AFTER the merge; then the cut.
    let merged = merge_claused(
        vec![
            hit(b"s1", 1, Some(b"m"), Some(b"g")),
            hit(b"s2", 2, Some(b"a"), Some(b"g")),
            hit(b"s3", 3, None, None),
            hit(b"s4", 4, Some(b"z"), Some(b"h")),
        ],
        Some(false),
        true,
        1,
        2,
    );
    // Order: s2(a) s1(m: collapsed away, same group g) s4(z) s3(none) →
    // collapsed [s2, s4, s3] → offset 1 → [s4, s3] → limit 2.
    assert_eq!(keys(&merged), [b"s4".to_vec(), b"s3".to_vec()]);
    // Past-the-end offset = empty, not an error.
    let merged = merge_claused(vec![hit(b"a", 1, None, None)], None, false, 99, 5);
    assert!(merged.is_empty());
}

#[test]
fn facet_fold_sums_by_identity_across_shards() {
    let mut acc: Vec<Vec<FacetBucket>> = vec![vec![]];
    fold_facets(&mut acc, vec![vec![(b"id1".to_vec(), b"1".to_vec(), 2)]]);
    fold_facets(
        &mut acc,
        vec![vec![(b"id1".to_vec(), b"1.0".to_vec(), 3), (b"id2".to_vec(), b"7".to_vec(), 9)]],
    );
    sort_facets(&mut acc);
    assert_eq!(
        acc[0],
        vec![
            (b"id2".to_vec(), b"7".to_vec(), 9),
            (b"id1".to_vec(), b"1".to_vec(), 5),
        ],
        "summed by identity; the label is the first spelling seen"
    );
}

#[test]
fn scalar_sorted_order_agrees_with_the_text_contract() {
    use std::cmp::Ordering;
    let o = scalar_sorted_order;
    assert_eq!(o((Some(b"a"), b"k1"), (Some(b"b"), b"k2"), false), Ordering::Less);
    assert_eq!(o((Some(b"a"), b"k1"), (Some(b"b"), b"k2"), true), Ordering::Greater);
    // A row WITH a value outranks one without, in BOTH directions.
    assert_eq!(o((Some(b"z"), b"k1"), (None, b"k2"), false), Ordering::Less);
    assert_eq!(o((Some(b"z"), b"k1"), (None, b"k2"), true), Ordering::Less);
    // Ties break by row key so the merged page is stable.
    assert_eq!(o((Some(b"a"), b"k1"), (Some(b"a"), b"k2"), true), Ordering::Less);
    assert_eq!(o((None, b"k1"), (None, b"k2"), true), Ordering::Less);
}

/// count_claused = the length of the claused query's full result,
/// without pages — pinned against the query itself.
#[test]
fn count_claused_matches_the_query_total() {
    let mut seg = Segment::with_values(1);
    for i in 0..500u32 {
        let key = format!("k{i:04}").into_bytes();
        let dept: &[u8] = if i % 3 == 0 { b"eng" } else { b"ops" };
        seg.apply_with_values(&key, Some(IndexValue::I64(i64::from(i))), &[Some(dept)]);
    }
    let eng = ValueTest::eq(ValType::Str, b"eng").unwrap();
    let filters = [(0usize, eng)];
    let n = seg.count_claused(&IndexValue::I64(0), &IndexValue::I64(499), &filters);
    let page = seg.query_claused(
        &IndexValue::I64(0),
        &IndexValue::I64(499),
        None,
        &ScalarClauses { filters: &filters, sort: None, distinct: None, facets: &[], fetch: 10_000 },
    );
    assert_eq!(n, page.hits.len() as u64);
    assert_eq!(n, 167, "0,3,...,498");
    // Unfiltered count_claused equals the plain count.
    assert_eq!(
        seg.count_claused(&IndexValue::I64(100), &IndexValue::I64(199), &[]),
        seg.count(&IndexValue::I64(100), &IndexValue::I64(199)),
    );
}
