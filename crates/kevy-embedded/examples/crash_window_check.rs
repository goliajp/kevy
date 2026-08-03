//! crashgate's windowed verifier: reopen the killed store (replay,
//! segment load, orphan sweep) and assert the R2c contract —
//!
//!   crash_window_check <dir> <synced> [--shards N]
//!
//! * loss-bound: every row at or before the last SYNCED barrier is
//!   readable and intact, wherever it lives (hot layer or a row
//!   segment — KV transparency is part of the contract);
//! * agreement: the rebuilt window index counts exactly the readable
//!   rows (a sampled census across the whole keyspace agrees with
//!   IDX.COUNT — the index, the row set and the segments cannot have
//!   torn apart);
//! * text: the rebuilt-and-refrozen text index answers a MATCH
//!   without refusing (its cold directory is derived spill and must
//!   self-heal).
//!
//! Exits 0 on success; panics (non-zero) on any violation.
use kevy_embedded::{Config, MatchOpts, Store};
use kevy_index::IndexValue;
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: crash_window_check <dir> <synced>");
    let synced: u64 = args.next().expect("synced").parse().expect("synced u64");
    let mut shards = 1usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--shards" => shards = args.next().unwrap().parse().unwrap(),
            other => panic!("unknown flag {other}"),
        }
    }
    let store = Store::open(
        Config::default()
            .with_persist(&dir)
            .with_shards(shards)
            .with_reaper_interval(Duration::from_millis(5)),
    )
    .expect("reopen after kill");

    // Loss bound: rows 1..=synced all answer, fields intact. Sampled
    // densely near the kill (the tail is where a torn slide would
    // bite) and sparsely across the body.
    let mut checked = 0u64;
    let mut n = 1u64;
    while n <= synced {
        let key = format!("r:{n}");
        let owned: Vec<Vec<u8>> =
            vec![b"HGET".to_vec(), key.clone().into_bytes(), b"at".to_vec()];
        let mut reply = Vec::new();
        store.dispatch_argv(&owned, &mut reply);
        let want = format!("${}\r\n{}\r\n", n.to_string().len(), n);
        assert_eq!(
            String::from_utf8_lossy(&reply),
            want,
            "row {key} lost or torn (synced barrier was {synced})"
        );
        checked += 1;
        // Every row in the last two spans, every 97th before that.
        n += if n + 120 >= synced { 1 } else { 97 };
    }
    assert!(checked > 0, "checked nothing — bad synced value {synced}?");

    // Agreement: the window index's whole-domain count equals a full
    // census of readable rows (which may exceed the barrier — rows
    // written after the last fsync are allowed to survive, never to
    // tear).
    let count = store
        .idx_count(b"ev.at", &IndexValue::I64(0), &IndexValue::I64(i64::MAX))
        .expect("count");
    assert!(count >= synced, "index count {count} below the loss bound {synced}");
    let mut census = 0u64;
    for n in 1..=count + 2_000 {
        let key = format!("r:{n}");
        let owned: Vec<Vec<u8>> = vec![b"EXISTS".to_vec(), key.into_bytes()];
        let mut reply = Vec::new();
        store.dispatch_argv(&owned, &mut reply);
        if reply == b":1\r\n" {
            census += 1;
        }
    }
    assert_eq!(count, census, "index and row set disagree after replay");

    // Text: the rebuilt index serves; the cold directory self-healed.
    let hits = store
        .idx_match_with(b"ev.note", b"event", 5, MatchOpts::default())
        .expect("text index must serve after replay");
    assert!(!hits.is_empty(), "text index lost every document");

    println!("OK synced={synced} count={count} checked={checked}");
}
