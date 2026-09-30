//! A global index with stored `VALUES` on one shard (its one partition is
//! the shard's own), and the deltas an owner is sent but does not take.

use kevy_index::{IndexKind, IndexSpec, IndexValue, Partitioning, ValType};
use kevy_rt::Commands;

use super::global::{Delta, GlobalRole};
use super::test_shard::{Shard, text};

const CREATE: &str =
    "IDX.CREATE g ON PREFIX u: FIELD age TYPE i64 KIND range VALUES city PARTITION global";

/// The number a VERIFY reply gives under `label`.
fn count(reply: &str, label: &str) -> String {
    let lines: Vec<&str> = reply.split("\r\n").collect();
    lines[lines.iter().position(|l| *l == label).unwrap() + 2].to_string()
}

#[test]
fn verify_waits_for_the_build_then_matches_entries_with_their_stored_values() {
    let mut s = Shard::new();
    s.hset("u:1", &[("age", "30"), ("city", "tokyo")]);
    // no city: the stored column is absent, which the entry hash tells apart
    s.hset("u:2", &[("age", "40")]);
    s.ok(CREATE);
    assert!(text(&s.ext("IDX.VERIFY g")).starts_with("-INDEXBUILDING"));
    s.settle();
    let reply = text(&s.ext("IDX.VERIFY g"));
    let got: Vec<String> =
        ["checked", "drift", "missing"].iter().map(|l| count(&reply, l)).collect();
    assert_eq!(got, ["2", "0", "0"], "{reply}");
}

#[test]
fn a_row_updated_in_place_moves_its_entry_and_a_key_that_is_not_a_hash_derives_none() {
    let mut s = Shard::new();
    s.ok(CREATE);
    s.settle();
    s.hset("u:1", &[("age", "30"), ("city", "tokyo")]);
    s.hset("u:1", &[("age", "31")]);
    assert_eq!(s.run("SET u:2 plain"), b"+OK\r\n");
    s.cmds.on_write(&mut s.store, b"u:2");
    s.settle();
    let page = text(&s.ext("IDX.QUERY g RANGE 0 100 FIELDS city"));
    assert!(page.contains("u:1") && page.contains("tokyo") && !page.contains("u:2"), "{page}");
    assert!(!text(&s.ext("IDX.QUERY g EQ 30")).contains("u:1"), "the old entry is gone");
    let reply = text(&s.ext("IDX.VERIFY g"));
    assert_eq!((count(&reply, "checked"), count(&reply, "drift")), ("1".into(), "0".into()));
    assert_eq!(s.run("DEL u:1"), b":1\r\n");
    s.cmds.on_write(&mut s.store, b"u:1");
    s.settle();
    assert!(!text(&s.ext("IDX.QUERY g RANGE 0 100")).contains("u:1"), "a deleted row leaves");
}

fn role() -> GlobalRole {
    let spec = IndexSpec::builder("g", "u:", IndexKind::Range, ValType::I64)
        .with_field("age")
        .build()
        .unwrap();
    GlobalRole::new(&spec, &Partitioning::Global { splits: Vec::new() }, (0, 1), 7)
}

#[test]
fn an_owner_ignores_a_delta_for_a_partition_it_does_not_own_and_a_shard_it_does_not_know() {
    let mut g = role();
    let upsert = || Delta::Upsert {
        key: b"u:1".to_vec(),
        old: None,
        value: IndexValue::I64(1),
        values: vec![],
    };
    g.apply(3, upsert());
    g.apply(0, Delta::Built { from: 99 });
    assert!(!g.ready(), "a shard that is not there does not finish the build");
    assert_eq!(g.owned[0].1.stats().entries, 0);
    g.apply(0, upsert());
    assert_eq!(g.owned[0].1.stats().entries, 1);
}

#[test]
fn a_delta_that_does_not_decode_or_names_no_index_here_changes_nothing() {
    let mut s = Shard::new();
    s.ok(CREATE);
    s.hset("u:1", &[("age", "30")]);
    s.settle();
    let ctx = s.cmds.ctx();
    super::apply_ext(&ctx, b"not a delta");
    let upsert = Delta::Upsert {
        key: b"u:9".to_vec(),
        old: None,
        value: IndexValue::I64(9),
        values: vec![None],
    };
    super::apply_ext(&ctx, &super::global_wire::encode(b"nosuch", 1, 0, &upsert));
    super::apply_ext(&ctx, &super::global_wire::encode(b"g", u64::MAX, 0, &upsert));
    let page = text(&s.ext("IDX.QUERY g RANGE 0 100"));
    assert!(page.contains("u:1") && !page.contains("u:9"), "{page}");
}
