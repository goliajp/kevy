//! The embedded face's sliding-window gate — the server e2e's mirror:
//! a windowed table and a control table over ONE key prefix must
//! answer every range/COUNT identically after the reaper has slid the
//! windowed index's cold prefix out, including after cold-row churn;
//! clauses on the cold index refuse by name; index memory really
//! shrinks. One shared runtime crate serves both faces, and this is
//! the proof it behaves the same from here.

#![cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]

use std::time::{Duration, Instant};

use kevy_embedded::{Config, Store};

/// Dispatch one command, panicking on an -ERR reply.
fn run(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
    assert!(!out.starts_with(b"-ERR"), "{}", String::from_utf8_lossy(&out));
    out
}
use kevy_index::{IndexKind, IndexValue, TableIndex, TableSpec, ValType, WindowSpec};

fn table(name: &[u8], windowed: bool) -> TableSpec {
    TableSpec {
        name: name.to_vec(),
        prefix: b"ev:".to_vec(),
        pk: b"id".to_vec(),
        columns: vec![(b"id".to_vec(), ValType::Str), (b"at".to_vec(), ValType::I64)],
        indexes: vec![TableIndex {
            column: b"at".to_vec(),
            kind: IndexKind::Range,
            values: vec![b"at".to_vec()],
        }],
        orderpaths: vec![],
        window: windowed.then_some(WindowSpec {
            column: b"at".to_vec(),
            span: 100,
            bucket: 10,
        }),
    }
}

fn v(i: i64) -> IndexValue {
    IndexValue::I64(i)
}

#[test]
fn embedded_window_slides_and_stays_semantically_equivalent() {
    let d = kevy_tmpdir::TmpDir::new("emb-winscalar");
    let s = Store::open(
        Config::default()
            .with_persist(d.path())
            .with_reaper_interval(Duration::from_millis(25)),
    )
    .expect("open");

    s.table_declare(table(b"ev", true)).expect("declare ev");
    s.table_declare(table(b"ctl", false)).expect("declare ctl");
    for i in 0..30i64 {
        let key = format!("ev:{i}");
        let at = (i * 10).to_string();
        run(&s, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(), b"at", at.as_bytes()]);
    }

    // Wait for the reaper's slide: a derived segment appears.
    let segs = d.path().join("segs-0");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let slid = segs.exists()
            && std::fs::read_dir(&segs).is_ok_and(|r| {
                r.filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().ends_with(".seg"))
            });
        if slid {
            break;
        }
        assert!(Instant::now() < deadline, "embedded window never slid");
        std::thread::sleep(Duration::from_millis(25));
    }

    // Memory really shrank: the windowed index's hot tree holds only
    // the in-window entries, the control still holds all 30.
    let ev = s.idx_stats(b"ev.at").expect("stats ev");
    let ctl = s.idx_stats(b"ctl.at").expect("stats ctl");
    assert_eq!(ctl.entries, 30);
    assert!(ev.entries < 30, "nothing left the hot tree: {}", ev.entries);
    assert!(ev.approx_bytes < ctl.approx_bytes);

    let compare = |tag: &str| {
        for (lo, hi) in [(-1000, 1000), (0, 100), (150, 250), (200, 300), (400, 500), (50, 50)] {
            let c_ev = s.idx_count(b"ev.at", &v(lo), &v(hi)).expect("count ev");
            let c_ctl = s.idx_count(b"ctl.at", &v(lo), &v(hi)).expect("count ctl");
            assert_eq!(c_ev, c_ctl, "{tag}: COUNT {lo}..{hi}");
            let q_ev = s.idx_query(b"ev.at", &v(lo), &v(hi), None, 100).expect("query ev");
            let q_ctl = s.idx_query(b"ctl.at", &v(lo), &v(hi), None, 100).expect("query ctl");
            assert_eq!(q_ev.0, q_ctl.0, "{tag}: QUERY {lo}..{hi}");
        }
    };
    compare("after slide");
    assert_eq!(s.idx_count(b"ev.at", &v(-1000), &v(1000)).unwrap(), 30);

    // Cold-row churn: rewrite (same value), delete, revive in-window.
    run(&s, &[b"HSET", b"ev:5", b"id", b"ev:5", b"at", b"50"]);
    run(&s, &[b"DEL", b"ev:7"]);
    run(&s, &[b"HSET", b"ev:3", b"id", b"ev:3", b"at", b"260"]);
    compare("after cold-row churn");
    assert_eq!(s.idx_count(b"ev.at", &v(-1000), &v(1000)).unwrap(), 29);

    // Clauses on the cold index refuse by name.
    let err = s
        .idx_count_claused(
            b"ev.at",
            &v(0),
            &v(300),
            &[kevy_embedded::ValueFilter::Range { field: b"at", min: b"100", max: b"300" }],
        )
        .expect_err("cold clause must refuse");
    assert!(format!("{err}").contains("not built yet"), "{err}");
}
