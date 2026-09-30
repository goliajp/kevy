//! Each way `VIEW.CREATE` refuses a declaration, `VIEW.DROP` of a view
//! that is not there, and a materialized view's `TOPK` cut.

use crate::index_runtime::test_shard::{Shard, text};

fn shard() -> Shard {
    let mut s = Shard::new();
    s.ok("IDX.CREATE age ON PREFIX user: FIELD age TYPE i64 KIND range");
    s
}

#[test]
fn every_refusal_names_what_the_declaration_got_wrong() {
    let mut s = shard();
    let cases = [
        ("VIEW.CREATE v QUERY nosuch RANGE 1 2 ORDER BY age", "view leaf references unknown index"),
        ("VIEW.CREATE v QUERY age RANGE 1 2 SORT BY age", "ORDER BY <index> is required"),
        ("VIEW.CREATE v QUERY age EQ 1 ORDER BY", "ORDER BY <index> is required"),
        ("VIEW.CREATE v QUERY age RANGE 1 2 ORDER BY nosuch", "ORDER BY references unknown index"),
        ("VIEW.CREATE v QUERY age RANGE 1 2 ORDER BY age MODE bogus", "MODE must be"),
        (
            "VIEW.CREATE v QUERY age RANGE 1 2 ORDER BY age TOPK 5",
            "TOPK requires MODE materialized",
        ),
    ];
    for (line, why) in cases {
        let reply = text(&s.run(line));
        assert!(reply.starts_with("-ERR ") && reply.contains(why), "{line}: {reply}");
    }
    assert!(s.cmds.state().catalogs.view().is_none_or(|c| c.is_empty()), "nothing was declared");
    s.ok("VIEW.CREATE v QUERY age RANGE 1 2 ORDER BY age");
    let again = text(&s.run("VIEW.CREATE v QUERY age RANGE 3 4 ORDER BY age"));
    assert!(again.starts_with("-") && again.contains("exists"), "{again}");
}

#[test]
fn dropping_a_view_that_is_not_there_answers_zero() {
    let mut s = shard();
    assert_eq!(s.run("VIEW.DROP nosuch"), b":0\r\n");
    s.ok("VIEW.CREATE v QUERY age RANGE 1 2 ORDER BY age");
    assert_eq!(s.run("VIEW.DROP v"), b":1\r\n");
}

/// The keys a view holds, in its order.
fn members(s: &mut Shard, view: &[u8]) -> Vec<String> {
    s.settle();
    let page = crate::view_runtime::shard_page(&s.cmds.ctx(), view, None, usize::MAX).unwrap();
    page.into_iter().map(|(_, k)| String::from_utf8(k).unwrap()).collect()
}

#[test]
fn a_topk_view_keeps_the_rows_at_the_end_its_order_starts_from() {
    let mut s = shard();
    for age in 1..=8 {
        s.hset(&format!("user:{age}"), &[("age", &age.to_string())]);
    }
    let q = "QUERY age RANGE 0 100 ORDER BY age";
    s.ok(&format!("VIEW.CREATE low {q} MODE materialized TOPK 4"));
    s.ok(&format!("VIEW.CREATE high {q} DESC MODE materialized TOPK 4"));
    // four kept, plus a quarter of four as slack
    assert_eq!(members(&mut s, b"low"), ["user:1", "user:2", "user:3", "user:4", "user:5"]);
    assert_eq!(members(&mut s, b"high"), ["user:8", "user:7", "user:6", "user:5", "user:4"]);
}
