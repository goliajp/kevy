//! Indexes and materialized views stay equal to the rows across the less
//! common shapes of a write: a row change still unapplied when another
//! index is declared, a backfill longer than one page, and the multi-key
//! verbs a view's write hook walks.

use std::time::Duration;

use crate::{Config, IndexKind, IndexValType, IndexValue, Store, ViewLeaf, ViewMode, ViewTree};

fn store() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

fn keys_in(s: &Store, index: &[u8], min: IndexValue, max: IndexValue) -> Vec<Vec<u8>> {
    let (hits, _) = s.idx_query(index, &min, &max, None, 100_000).unwrap();
    let mut keys: Vec<Vec<u8>> = hits.into_iter().map(|(k, _)| k).collect();
    keys.sort();
    keys
}

fn every_i64(s: &Store, index: &[u8]) -> Vec<Vec<u8>> {
    keys_in(s, index, IndexValue::I64(i64::MIN), IndexValue::I64(i64::MAX))
}

fn every_str(s: &Store, index: &[u8]) -> Vec<Vec<u8>> {
    keys_in(s, index, IndexValue::Str(Vec::new()), IndexValue::Str(vec![0xff; 8]))
}

fn rows(s: &Store, pattern: &[u8]) -> Vec<Vec<u8>> {
    let mut keys = s.keys(Some(pattern), None);
    keys.sort();
    keys
}

/// Expire `key` and let a read reap it: the store records the change, and
/// nothing applies it to the indexes until their next drain.
fn reap(s: &Store, key: &[u8]) {
    s.expire(key, Duration::from_millis(5)).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(s.hget(key, b"n").unwrap(), None);
}

#[test]
fn an_index_declared_over_an_unapplied_row_change_agrees_with_the_rows() {
    let s = store();
    for i in 0..10 {
        let row = format!("{i}");
        s.hset(format!("ix:{i}").as_bytes(), &[(b"n", row.as_bytes()), (b"f", b"x")]).unwrap();
        s.hset(format!("ot:{i}").as_bytes(), &[(b"n", row.as_bytes())]).unwrap();
    }
    s.idx_create(b"ix_n", b"ix:", b"n", IndexValType::I64, IndexKind::Range).unwrap();

    // a new field on a prefix already watched
    reap(&s, b"ix:1");
    s.idx_create(b"ix_f", b"ix:", b"f", IndexValType::Str, IndexKind::Range).unwrap();
    assert_eq!(every_i64(&s, b"ix_n"), rows(&s, b"ix:*"));
    assert_eq!(every_str(&s, b"ix_f"), rows(&s, b"ix:*"));

    // a prefix nothing watched yet
    reap(&s, b"ix:2");
    s.idx_create(b"ot_n", b"ot:", b"n", IndexValType::I64, IndexKind::Range).unwrap();
    assert_eq!(every_i64(&s, b"ix_n"), rows(&s, b"ix:*"));
    assert_eq!(every_str(&s, b"ix_f"), rows(&s, b"ix:*"));
    assert_eq!(every_i64(&s, b"ot_n"), rows(&s, b"ot:*"));
    assert_eq!(rows(&s, b"ix:*").len(), 8);
}

#[test]
fn an_index_declared_over_more_rows_than_one_backfill_page_takes_every_row() {
    let s = store();
    for i in 0..6000 {
        s.hset(format!("bf:{i}").as_bytes(), &[(b"n", format!("{i}").as_bytes())]).unwrap();
    }
    s.idx_create(b"bf_n", b"bf:", b"n", IndexValType::I64, IndexKind::Range).unwrap();
    assert_eq!(s.idx_count(b"bf_n", &IndexValue::I64(0), &IndexValue::I64(5999)).unwrap(), 6000);
}

fn members(s: &Store, view: &[u8]) -> Vec<String> {
    let (page, _) = s.view_query(view, None, 100).unwrap();
    let mut keys: Vec<String> =
        page.into_iter().map(|(k, _)| String::from_utf8_lossy(&k).into_owned()).collect();
    keys.sort();
    keys
}

/// Every `u:` row with an `age` of 18 or more, materialized.
fn adults(s: &Store) {
    for (i, age) in [30, 40, 50, 60, 10].into_iter().enumerate() {
        s.hset(format!("u:{i}").as_bytes(), &[(b"age", age.to_string().as_bytes())]).unwrap();
    }
    s.idx_create(b"age", b"u:", b"age", IndexValType::I64, IndexKind::Range).unwrap();
    let tree = ViewTree::Leaf(ViewLeaf::new("age", IndexValue::I64(18), IndexValue::I64(200)));
    let mode = ViewMode::Materialized { top_k: 0 };
    s.view_create(b"adults", tree, b"age", kevy_index::SortOrder::Asc, mode).unwrap();
    assert_eq!(members(s, b"adults"), ["u:0", "u:1", "u:2", "u:3"]);
}

#[test]
fn a_materialized_view_follows_the_multi_key_writes() {
    let s = store();
    adults(&s);
    s.del(&[&b"u:0"[..], b"u:4"]).unwrap();
    assert_eq!(members(&s, b"adults"), ["u:1", "u:2", "u:3"], "DEL");
    s.mset(&[(&b"u:1"[..], &b"a string now"[..]), (b"x", b"y")]).unwrap();
    assert_eq!(members(&s, b"adults"), ["u:2", "u:3"], "MSET");
    s.rename(b"u:2", b"u:20").unwrap();
    assert_eq!(members(&s, b"adults"), ["u:20", "u:3"], "RENAME");
}
