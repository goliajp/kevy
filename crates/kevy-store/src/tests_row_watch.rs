//! The row recorder, path by path: every way a row under a watched prefix
//! changes is recorded once with the fields it held before, and nothing
//! that leaves a row's content alone is.

use core::time::Duration;

use crate::{EvictionPolicy, HExpireCond, RowChanges, RowWatch, SetCondition, Store};

fn watched() -> Store {
    let mut s = Store::new();
    s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"a".to_vec(), b"b".to_vec()]));
    s
}

/// What was recorded, as `(key, was a hash, a, b)`.
type Row = (Vec<u8>, bool, Option<Vec<u8>>, Option<Vec<u8>>);
type Seen = Vec<Row>;

fn take(s: &mut Store) -> Seen {
    let c = s.take_row_changes(RowChanges::default());
    assert!(!c.is_reset());
    c.iter()
        .map(|r| {
            (
                r.key().to_vec(),
                r.was_hash(),
                r.field(0, 0).map(<[u8]>::to_vec),
                r.field(0, 1).map(<[u8]>::to_vec),
            )
        })
        .collect()
}

fn row(k: &str, a: Option<&str>, b: Option<&str>) -> Row {
    (
        k.as_bytes().to_vec(),
        true,
        a.map(|x| x.as_bytes().to_vec()),
        b.map(|x| x.as_bytes().to_vec()),
    )
}

fn seeded() -> Store {
    let mut s = watched();
    s.hset(b"u:1", &[(b"a", b"1"), (b"b", b"x"), (b"c", b"pad")]).expect("hash");
    s.hset(b"other:1", &[(b"a", b"1")]).expect("hash");
    take(&mut s);
    s
}

#[test]
fn a_new_row_was_absent() {
    let mut s = watched();
    s.hset(b"u:1", &[(b"a", b"1")]).expect("hash");
    assert_eq!(take(&mut s), [(b"u:1".to_vec(), false, None, None)]);
}

#[test]
fn field_writes_record_the_old_fields_once() {
    let mut s = seeded();
    s.hset(b"u:1", &[(b"a", b"2")]).expect("hash");
    s.hdel(b"u:1", &[&b"b"[..]]).expect("hash");
    s.hincrby(b"u:1", b"a", 5).expect("an integer");
    s.hsetnx(b"u:1", b"z", b"new").expect("hash");
    assert_eq!(take(&mut s), [row("u:1", Some("1"), Some("x"))]);
    s.hincrbyfloat(b"u:1", b"a", 0.5).expect("a number");
    assert_eq!(take(&mut s), [row("u:1", Some("7"), None)], "each take starts afresh");
}

#[test]
fn rows_outside_every_prefix_are_not_recorded() {
    let mut s = seeded();
    s.hset(b"other:1", &[(b"a", b"2")]).expect("hash");
    s.del(&[&b"other:1"[..]]);
    assert!(take(&mut s).is_empty());
    assert!(!s.has_row_changes());
}

#[test]
fn deleting_and_overwriting_record_the_row() {
    let mut s = seeded();
    s.del(&[&b"u:1"[..]]);
    assert_eq!(take(&mut s), [row("u:1", Some("1"), Some("x"))]);
    let mut s = seeded();
    s.set(b"u:1", b"a string now".to_vec(), None, SetCondition::Always);
    s.set(b"u:1", b"and again".to_vec(), None, SetCondition::Always);
    assert_eq!(take(&mut s), [row("u:1", Some("1"), Some("x"))]);
    s.hset(b"u:2", &[(b"a", b"1")]).expect("hash");
    s.del(&[&b"u:1"[..]]);
    let seen = take(&mut s);
    assert_eq!(seen[1], (b"u:1".to_vec(), false, None, None), "a string is not a row");
}

#[test]
fn rename_records_both_names() {
    let mut s = seeded();
    s.hset(b"u:2", &[(b"a", b"2")]).expect("hash");
    take(&mut s);
    s.rename(b"u:1", b"u:2");
    let mut seen = take(&mut s);
    seen.sort();
    assert_eq!(seen, [row("u:1", Some("1"), Some("x")), row("u:2", Some("2"), None)]);
}

#[test]
fn ttl_changes_record_the_unchanged_row() {
    let mut s = seeded();
    s.expire(b"u:1", Duration::from_secs(100));
    s.persist(b"u:1");
    assert_eq!(take(&mut s), [row("u:1", Some("1"), Some("x"))]);
}

#[test]
fn expiry_records_the_row_it_drops() {
    // a deadline already past deletes at once
    let mut s = seeded();
    s.expire_at_unix_ms(b"u:1", 1);
    assert_eq!(take(&mut s), [row("u:1", Some("1"), Some("x"))]);
    // lazy: a read of an expired row, and active: the sampler
    for active in [false, true] {
        let mut s = seeded();
        s.expire(b"u:1", Duration::from_millis(5));
        take(&mut s);
        std::thread::sleep(Duration::from_millis(10));
        if active {
            // the sampler starts at a random bucket; give it a few ticks
            for _ in 0..64 {
                s.tick_expire(20, 16);
                if s.dbsize() == 1 {
                    break;
                }
            }
            assert_eq!(s.dbsize(), 1, "the sampler reached the row");
        } else {
            assert_eq!(s.hget(b"u:1", b"a").expect("hash"), None);
        }
        assert_eq!(take(&mut s), [row("u:1", Some("1"), Some("x"))], "active={active}");
    }
}

#[test]
fn field_expiry_records_the_row_on_read_and_on_the_sweep() {
    for sweep in [false, true] {
        let mut s = seeded();
        s.hexpire_at(b"u:1", &[&b"b"[..]], crate::now_unix_ms() + 5, HExpireCond::Always)
            .expect("hash");
        take(&mut s);
        std::thread::sleep(Duration::from_millis(10));
        if sweep {
            s.tick_hash_ttl(16);
        } else {
            s.hget(b"u:1", b"a").expect("hash");
        }
        assert_eq!(take(&mut s), [row("u:1", Some("1"), Some("x"))], "sweep={sweep}");
        assert_eq!(s.hget(b"u:1", b"b").expect("hash"), None);
    }
}

#[test]
fn eviction_records_the_row_it_drops() {
    let mut s = seeded();
    s.set_max_memory(1, EvictionPolicy::AllKeysRandom);
    s.try_evict_after_write();
    let seen = take(&mut s);
    assert!(seen.contains(&row("u:1", Some("1"), Some("x"))), "{seen:?}");
}

#[test]
fn flush_starts_over_and_lists_what_came_after() {
    let mut s = seeded();
    s.hset(b"u:2", &[(b"a", b"1")]).expect("hash");
    s.flushall();
    s.hset(b"u:3", &[(b"a", b"1")]).expect("hash");
    let c = s.take_row_changes(RowChanges::default());
    assert!(c.is_reset());
    let after: Vec<(&[u8], bool)> = c.iter().map(|r| (r.key(), r.was_hash())).collect();
    assert_eq!(after, [(&b"u:3"[..], false)], "the rows written after it, from nothing");
    let mut s = seeded();
    drop(s.detach_entries());
    assert!(s.take_row_changes(RowChanges::default()).is_reset());
}

#[test]
fn a_row_under_two_prefixes_carries_both_rules() {
    let mut s = Store::new();
    s.hset(b"u:vip:1", &[(b"a", b"1"), (b"tier", b"gold")]).expect("hash");
    s.set_row_watch(
        RowWatch::new()
            .with_prefix("u:", vec![b"a".to_vec()])
            .with_prefix("u:vip:", vec![b"tier".to_vec()]),
    );
    s.del(&[&b"u:vip:1"[..]]);
    let c = s.take_row_changes(RowChanges::default());
    let r = c.iter().next().expect("one row");
    assert_eq!((r.field(0, 0), r.field(1, 0)), (Some(&b"1"[..]), Some(&b"gold"[..])));
}

#[test]
fn many_rows_are_each_recorded_once() {
    let mut s = watched();
    for round in 0..3 {
        for i in 0..200 {
            s.hset(format!("u:{i}").as_bytes(), &[(b"a", format!("{round}").as_bytes())])
                .expect("hash");
        }
    }
    let seen = take(&mut s);
    assert_eq!(seen.len(), 200);
    assert!(seen.iter().all(|r| !r.1), "each recorded as absent, before its first write");
}

#[test]
fn the_installed_watch_is_the_one_read_back() {
    let mut s = Store::new();
    assert!(s.row_watch().is_none());
    let w = RowWatch::new().with_prefix("u:", vec![b"a".to_vec()]).with_prefix("v:", Vec::new());
    s.set_row_watch(w.clone());
    assert_eq!(s.row_watch(), Some(&w));
    assert_eq!(s.row_watch().map(RowWatch::rules), Some(2));
}

#[test]
fn installing_the_same_watch_again_keeps_what_was_recorded() {
    let mut s = watched();
    s.hset(b"u:1", &[(b"a", b"1")]).expect("hash");
    s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"a".to_vec(), b"b".to_vec()]));
    let c = s.take_row_changes(RowChanges::default());
    assert_eq!(c.len(), 1);
    assert!(!c.is_empty());
}

#[test]
fn without_a_watch_a_take_hands_back_the_spare() {
    let mut s = Store::new();
    s.hset(b"u:1", &[(b"a", b"1")]).expect("hash");
    let c = s.take_row_changes(RowChanges::default());
    assert!(c.is_empty());
    assert_eq!(c.len(), 0);
}

#[test]
fn a_row_under_one_rule_carries_no_fields_for_the_others() {
    let mut s = Store::new();
    s.set_row_watch(
        RowWatch::new()
            .with_prefix("u:", vec![b"a".to_vec()])
            .with_prefix("v:", vec![b"a".to_vec(), b"b".to_vec()]),
    );
    s.hset(b"v:1", &[(b"a", b"1"), (b"b", b"2")]).expect("hash");
    s.take_row_changes(RowChanges::default());
    s.hset(b"v:1", &[(b"a", b"3")]).expect("hash");
    let c = s.take_row_changes(RowChanges::default());
    let r = c.iter().next().expect("one row");
    assert_eq!(r.field(0, 0), None, "not under u:");
    assert_eq!((r.field(1, 0), r.field(1, 1)), (Some(&b"1"[..]), Some(&b"2"[..])));
    assert_eq!(r.field(1, 9), None, "past the rule's fields");
}

#[test]
fn a_field_past_the_rule_is_none_and_not_the_next_rows() {
    let mut s = Store::new();
    s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"a".to_vec()]));
    s.hset(b"u:1", &[(b"a", b"1")]).expect("hash");
    s.hset(b"u:2", &[(b"a", b"2")]).expect("hash");
    s.take_row_changes(RowChanges::default());
    s.hset(b"u:1", &[(b"a", b"3")]).expect("hash");
    s.hset(b"u:2", &[(b"a", b"4")]).expect("hash");
    let c = s.take_row_changes(RowChanges::default());
    let rows: Vec<_> = c.iter().map(|r| (r.field(0, 0), r.field(0, 1))).collect();
    assert_eq!(rows, [(Some(&b"1"[..]), None), (Some(&b"2"[..]), None)]);
}

#[test]
fn a_field_past_one_rule_is_none_and_not_the_next_rules() {
    let mut s = Store::new();
    s.set_row_watch(
        RowWatch::new()
            .with_prefix("u:", vec![b"a".to_vec()])
            .with_prefix("u:x", vec![b"b".to_vec()]),
    );
    s.hset(b"u:x1", &[(b"a", b"1"), (b"b", b"2")]).expect("hash");
    s.take_row_changes(RowChanges::default());
    s.hset(b"u:x1", &[(b"a", b"3")]).expect("hash");
    let c = s.take_row_changes(RowChanges::default());
    let r = c.iter().next().expect("one row");
    assert_eq!((r.field(0, 0), r.field(1, 0)), (Some(&b"1"[..]), Some(&b"2"[..])));
    assert_eq!(r.field(0, 1), None, "past rule 0's fields");
}

#[test]
fn a_sharded_row_records_its_old_fields() {
    let mut s = watched();
    let fields: Vec<Vec<u8>> =
        (0..crate::seg_map::HS_PROMOTE + 1).map(|i| format!("f{i}").into_bytes()).collect();
    let mut pairs: Vec<(&[u8], &[u8])> = fields.iter().map(|f| (&f[..], &b"v"[..])).collect();
    pairs.push((b"a", b"1"));
    s.hset(b"u:1", &pairs).expect("hash");
    assert!(matches!(
        s.map.get(b"u:1".as_slice()).map(|e| &e.value),
        Some(crate::Value::SegHash(_))
    ));
    take(&mut s);
    s.hset(b"u:1", &[(b"a", b"2")]).expect("hash");
    assert_eq!(take(&mut s), [row("u:1", Some("1"), None)]);
}

#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
mod cold {
    use super::*;

    fn tiered_seeded() -> (Store, kevy_tmpdir::TmpDir) {
        let d = kevy_tmpdir::TmpDir::new("row-watch-cold");
        let mut s = Store::new();
        s.enable_tiering(d.path(), 1 << 30).expect("a tier");
        s.set_row_watch(RowWatch::new().with_prefix("u:", vec![b"a".to_vec(), b"b".to_vec()]));
        s.hset(b"u:1", &[(b"a", b"1"), (b"b", b"x"), (b"c", &[7u8; 300])]).expect("hash");
        take(&mut s);
        assert!(s.demote_in_place(b"u:1"));
        (s, d)
    }

    #[test]
    fn demotion_is_not_a_write() {
        let (mut s, _d) = tiered_seeded();
        assert!(take(&mut s).is_empty());
        assert_eq!(s.hget(b"u:1", b"a").expect("hash"), Some(&b"1"[..]));
        assert!(take(&mut s).is_empty(), "nor is reading it back");
    }

    #[test]
    fn a_cold_row_is_read_back_for_its_old_fields() {
        let (mut s, _d) = tiered_seeded();
        s.del(&[&b"u:1"[..]]);
        assert_eq!(take(&mut s), [row("u:1", Some("1"), Some("x"))], "deleted cold");
        let (mut s, _d) = tiered_seeded();
        s.hset(b"u:1", &[(b"a", b"2")]).expect("hash");
        assert_eq!(take(&mut s), [row("u:1", Some("1"), Some("x"))], "written cold");
    }
}
