//! The window-narrowing observation end to end on the embedded face:
//! a windowed table slides, near-boundary queries leave a margin, and
//! `idx_advise` renders the bucket-aligned SPAN suggestion — until a
//! deep query erases the margin and the suggestion with it.

#![cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]

use std::time::{Duration, Instant};

use kevy_embedded::{Config, Store};
use kevy_index::{IndexKind, IndexValue, TableIndex, TableSpec, ValType, WindowSpec};

fn run(s: &Store, argv: &[&[u8]]) {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
}

fn windowed_table() -> TableSpec {
    TableSpec {
        name: b"ev".to_vec(),
        prefix: b"ev:".to_vec(),
        pk: b"id".to_vec(),
        columns: vec![(b"id".to_vec(), ValType::Str), (b"at".to_vec(), ValType::I64)],
        indexes: vec![TableIndex {
            column: b"at".to_vec(),
            kind: IndexKind::Range,
            values: vec![],
        }],
        orderpaths: vec![],
        window: Some(WindowSpec { column: b"at".to_vec(), span: 100, bucket: 10 }),
        autodeclare: 0,
        auto_added: vec![],
    }
}

fn wait_for_row_segment(dir: &std::path::Path) {
    let segs = dir.join("segs-0");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let slid = segs.exists() && std::fs::read_dir(&segs).is_ok_and(|r| r.count() > 0);
        if slid {
            return;
        }
        assert!(Instant::now() < deadline, "window never slid");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A Manual-mode store's windowed table slides on the caller-driven
/// tick — the cadence the background reaper would otherwise supply.
/// Deterministic: no sleeps, just ticks.
#[test]
fn manual_tick_drives_the_window_slide() {
    let d = kevy_tmpdir::TmpDir::new("emb-winmanual");
    let s = Store::open(Config::default().with_persist(d.path()).with_ttl_reaper_manual())
        .expect("open");
    s.table_declare(windowed_table()).expect("declare");
    for i in 0..30i64 {
        let key = format!("ev:{i}");
        let at = (i * 10).to_string();
        run(&s, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(), b"at", at.as_bytes()]);
    }
    let segs = d.path().join("segs-0");
    let slid = (0..200).any(|_| {
        s.tick();
        segs.exists() && std::fs::read_dir(&segs).is_ok_and(|r| r.count() > 0)
    });
    assert!(slid, "a Manual-mode windowed table must slide on tick, not stay all-hot");
    // And the evicted half still answers through the cold merge.
    let n = s
        .idx_count(b"ev.at", &IndexValue::I64(0), &IndexValue::I64(300))
        .expect("count across the window");
    assert_eq!(n, 30, "hot + cold together still hold every row");
}

#[test]
fn near_boundary_queries_earn_a_span_suggestion_deep_ones_erase_it() {
    let d = kevy_tmpdir::TmpDir::new("emb-winnarrow");
    let s = Store::open(
        Config::default().with_persist(d.path()).with_reaper_interval(Duration::from_millis(25)),
    )
    .expect("open");
    s.table_declare(windowed_table()).expect("declare");
    for i in 0..30i64 {
        let key = format!("ev:{i}");
        let at = (i * 10).to_string();
        run(&s, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(), b"at", at.as_bytes()]);
    }
    wait_for_row_segment(d.path());

    // A near-boundary query leaves a wide margin — the suggestion
    // appears, bucket-aligned.
    let (near, far) = (IndexValue::I64(250), IndexValue::I64(300));
    s.idx_query(b"ev.at", &near, &far, None, 100).expect("near query");
    let narrow: Vec<String> = s
        .idx_advise()
        .into_iter()
        .filter(|a| a.advice.starts_with("WINDOW"))
        .map(|a| a.advice)
        .collect();
    assert_eq!(narrow.len(), 1, "{narrow:?}");
    assert!(narrow[0].starts_with("WINDOW at SPAN 100 —"), "{}", narrow[0]);
    assert!(narrow[0].contains("still serves them"), "{}", narrow[0]);

    // A deep query probes the cold side; the margin — and the
    // suggestion — are gone for good.
    s.idx_query(b"ev.at", &IndexValue::I64(0), &far, None, 100).expect("deep query");
    assert!(
        s.idx_advise().iter().all(|a| !a.advice.starts_with("WINDOW")),
        "a cold-probing workload must not be advised to narrow"
    );
}
