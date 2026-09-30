//! Index reads on one shard, end to end through the fan-out halves:
//! COMPOSE's three ways to test B, VERIFY on a windowed index, a
//! global index's refusals, and MATCH's second phase.

use std::collections::BTreeSet;

use crate::index_runtime::test_shard::{Shard, text};

/// The row keys a reply names.
fn keys(reply: &[u8]) -> BTreeSet<String> {
    text(reply).split("\r\n").filter(|l| l.starts_with("r:")).map(str::to_string).collect()
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

fn rows(s: &mut Shard) {
    for i in 0..40 {
        s.hset(&format!("r:{i}"), &[("a", &i.to_string()), ("b", &(i % 10).to_string())]);
    }
}

#[test]
fn compose_refuses_bounds_that_do_not_coerce_on_either_side() {
    let mut s = Shard::new();
    s.ok("IDX.CREATE a ON PREFIX r: FIELD a TYPE i64 KIND range");
    s.ok("IDX.CREATE b ON PREFIX r: FIELD b TYPE i64 KIND range");
    rows(&mut s);
    s.settle();
    for line in [
        "IDX.QUERY COMPOSE AND a RANGE x 9 b RANGE 0 9",
        "IDX.QUERY COMPOSE AND a RANGE 0 9 b EQ x",
    ] {
        assert!(text(&s.ext(line)).starts_with("-ERR"), "{line}");
    }
    let both = s.ext("IDX.QUERY COMPOSE AND a RANGE 0 9 b EQ 3");
    assert_eq!(keys(&both), set(&["r:3"]));
}

#[test]
fn compose_and_asks_a_key_directory_when_a_view_keeps_one() {
    let mut s = Shard::new();
    s.ok("IDX.CREATE a ON PREFIX r: FIELD a TYPE i64 KIND range");
    s.ok("IDX.CREATE b ON PREFIX r: FIELD b TYPE i64 KIND range");
    s.ok("VIEW.CREATE low QUERY b RANGE 0 4 ORDER BY b");
    rows(&mut s);
    s.settle();
    let got = s.ext("IDX.QUERY COMPOSE AND a RANGE 10 19 b RANGE 3 4");
    assert_eq!(keys(&got), set(&["r:13", "r:14"]));
}

const WINDOWED: &str = "TABLE.DECLARE w PREFIX r: PK id COLUMN id i64 COLUMN a i64 COLUMN b i64 \
    INDEX a range INDEX b range WINDOW b SPAN 1000 BUCKET 100";

#[test]
fn compose_and_reads_a_windowed_b_row_by_row_and_keeps_only_what_b_holds() {
    let mut s = Shard::new();
    s.ok(WINDOWED);
    let now = kevy_store::now_unix_ms() / 1000;
    for i in 0..40u64 {
        s.hset(
            &format!("r:{i}"),
            &[("id", &i.to_string()), ("a", &i.to_string()), ("b", &now.to_string())],
        );
    }
    // an A-hit whose B value is outside B's range
    s.hset("r:1", &[("b", &(now - 500).to_string())]);
    s.settle();
    let (lo, hi) = (now - 10, now + 10);
    let line = format!("IDX.QUERY COMPOSE AND w.a RANGE 0 2 w.b RANGE {lo} {hi}");
    assert_eq!(keys(&s.ext(&line)), set(&["r:0", "r:2"]));
    // rows gone or no longer hashes, before any hook told the indexes
    assert_eq!(s.run("SET r:0 plain"), b"+OK\r\n");
    s.store.del(&[b"r:2".as_slice()]);
    assert_eq!(keys(&s.ext(&line)), set(&[]));
}

#[test]
fn verify_on_a_windowed_index_audits_its_window() {
    let mut s = Shard::new();
    s.ok(WINDOWED);
    let now = (kevy_store::now_unix_ms() / 1000).to_string();
    s.hset("r:1", &[("id", "1"), ("a", "1"), ("b", &now)]);
    s.settle();
    let reply = text(&s.ext("IDX.VERIFY w.b"));
    let lines: Vec<&str> = reply.split("\r\n").collect();
    let at = |label: &str| lines[lines.iter().position(|l| *l == label).unwrap() + 2];
    assert_eq!((at("checked"), at("drift")), ("1", "0"), "{reply}");
}

#[test]
fn a_global_index_refuses_fields_it_does_not_store_on_a_selecting_page() {
    let mut s = Shard::new();
    s.ok("IDX.CREATE g ON PREFIX u: FIELD age TYPE i64 KIND range VALUES city PARTITION global");
    s.hset("u:1", &[("age", "30"), ("city", "tokyo")]);
    s.settle();
    let stored = text(&s.ext("IDX.QUERY g RANGE 0 100 SORT city DESC FIELDS city"));
    assert!(stored.contains("tokyo"), "{stored}");
    let refused = text(&s.ext("IDX.QUERY g RANGE 0 100 SORT city DESC FIELDS name"));
    assert!(refused.starts_with("-ERR") && refused.contains("name"), "{refused}");
}

#[test]
fn match_scores_in_a_second_phase_against_the_counters_the_first_gathered() {
    let mut s = Shard::new();
    s.ok("IDX.CREATE body ON PREFIX r: FIELD body TYPE str KIND text");
    s.hset("r:1", &[("body", "red fox")]);
    s.hset("r:2", &[("body", "blue whale")]);
    s.hset("r:3", &[("body", "red whale")]);
    s.settle();
    assert_eq!(keys(&s.ext("IDX.QUERY body MATCH fox")), set(&["r:1"]));
    assert_eq!(keys(&s.ext("IDX.QUERY body MATCH red")), set(&["r:1", "r:3"]));
}
