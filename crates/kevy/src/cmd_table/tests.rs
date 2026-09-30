//! The table verbs' refusals and misses, and a table whose declaration
//! changes under indexes that stay.

use std::collections::HashMap;
use std::sync::Arc;

use crate::index_runtime::test_shard::{Shard, text, words};

const T: &str = "t PREFIX t: PK id COLUMN id i64 COLUMN a i64 COLUMN at i64";

#[test]
fn dropping_a_table_that_is_not_there_answers_zero() {
    let mut s = Shard::new();
    assert_eq!(s.run("TABLE.DROP nosuch"), b":0\r\n");
    s.ok(&format!("TABLE.DECLARE {T} INDEX a range"));
    assert_eq!(s.run("TABLE.DROP t"), b":1\r\n");
    assert!(s.cmds.state().catalogs.index().is_none_or(|c| c.get(b"t.a").is_none()));
}

#[test]
fn a_replacement_that_does_not_compile_leaves_the_old_table_standing() {
    let mut s = Shard::new();
    s.ok(&format!("TABLE.DECLARE {T} INDEX a range"));
    let reply = text(&s.run(&format!("TABLE.REPLACE {T} INDEX nosuch range")));
    assert!(reply.starts_with("-ERR") && reply.contains("nosuch"), "{reply}");
    assert!(s.cmds.state().catalogs.index().is_some_and(|c| c.get(b"t.a").is_some()));
}

#[test]
fn a_declaration_whose_path_is_already_an_index_is_refused() {
    let mut s = Shard::new();
    s.ok("IDX.CREATE t.a ON PREFIX t: FIELD a TYPE i64 KIND range");
    for line in [
        format!("TABLE.DECLARE {T} INDEX a range"),
        format!("TABLE.DECLARE {T} INDEX a range GLOBAL"),
        format!("TABLE.REPLACE {T} INDEX a range"),
    ] {
        let reply = text(&s.run(&line));
        assert!(reply.starts_with("-") && reply.contains("exists"), "{line}: {reply}");
    }
    assert!(s.cmds.state().catalogs.table().is_none_or(|c| c.get(b"t").is_none()));
}

#[test]
fn a_replacement_on_a_shard_whose_tier_floor_is_full_is_refused_for_all() {
    let s = Shard::new();
    let argv = words(&format!("TABLE.REPLACE {T} INDEX a range"));
    let blocked = vec![0, 1, 0, 0];
    let reply =
        crate::cmd_global_sample::reduce(&s.cmds.ctx(), &argv, &[vec![0, 0, 0, 0], blocked]);
    assert!(text(&reply).contains("index memory floor"), "{}", text(&reply));
    assert!(s.cmds.state().catalogs.table().is_none_or(|c| c.get(b"t").is_none()));
}

#[test]
fn ensure_refuses_a_table_whose_split_path_was_dropped_under_it() {
    let state = crate::RuntimeState::new(
        Arc::new(kevy_config::Config::default()),
        std::path::PathBuf::new(),
        2,
    )
    .unwrap();
    let mut s = Shard::with_state(state);
    let declare = format!("{T} INDEX a range GLOBAL SPLIT AT 5");
    s.ok(&format!("TABLE.DECLARE {declare}"));
    assert_eq!(s.run(&format!("TABLE.ENSURE {declare}")), b"+UNCHANGED\r\n");
    assert_eq!(s.run("IDX.DROP t.a"), b":1\r\n");
    let reply = text(&s.run(&format!("TABLE.ENSURE {declare}")));
    assert!(reply.contains("spread differently"), "{reply}");
}

/// The window each of the shard's indexes runs with, by name.
fn windows(s: &mut Shard) -> HashMap<String, (bool, bool)> {
    s.settle();
    crate::index_runtime::test_shard::window_states(&s.cmds.ctx())
        .into_iter()
        .map(|(n, w)| (String::from_utf8(n).unwrap(), w))
        .collect()
}

#[test]
fn a_window_added_by_replace_reaches_the_indexes_that_stay() {
    let mut s = Shard::new();
    s.ok(&format!("TABLE.DECLARE {T} COLUMN body str INDEX at range"));
    s.ok("IDX.CREATE t.body ON PREFIX t: FIELD body TYPE str KIND text");
    assert_eq!(windows(&mut s)["t.at"], (false, false));
    assert_eq!(windows(&mut s)["t.body"], (false, false));
    let windowed =
        format!("TABLE.REPLACE {T} COLUMN body str INDEX at range WINDOW at SPAN 100 BUCKET 10");
    s.ok(&windowed);
    assert_eq!(windows(&mut s)["t.at"], (true, false), "the scalar path slides");
    assert_eq!(windows(&mut s)["t.body"], (false, true), "the text path keeps a cold directory");
}

#[test]
fn table_verify_reads_a_windowed_path_and_counts_a_row_changed_behind_the_hook() {
    let mut s = Shard::new();
    s.ok(&format!("TABLE.DECLARE {T} INDEX at range WINDOW at SPAN 100 BUCKET 10"));
    let now = (kevy_store::now_unix_ms() / 1000).to_string();
    s.hset("t:1", &[("id", "1"), ("a", "1"), ("at", &now)]);
    s.hset("t:2", &[("id", "2"), ("a", "2"), ("at", &now)]);
    s.settle();
    let clean = text(&s.ext("TABLE.VERIFY t"));
    assert_eq!((count(&clean, "checked"), count(&clean, "drift")), (2, 0), "{clean}");
    s.store.hset(b"t:1", &[(b"at".as_slice(), b"not a number".as_slice())]).unwrap();
    let drifted = text(&s.ext("TABLE.VERIFY t"));
    assert_eq!((count(&drifted, "checked"), count(&drifted, "drift")), (2, 1), "{drifted}");
}

/// The number a VERIFY reply gives under `label`.
fn count(reply: &str, label: &str) -> u64 {
    let lines: Vec<&str> = reply.split("\r\n").collect();
    let at = lines.iter().position(|l| *l == label).unwrap();
    lines[at + 2].parse().unwrap()
}
