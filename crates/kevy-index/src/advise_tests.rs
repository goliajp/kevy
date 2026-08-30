//! [`super`]'s tests: family dedup and counting, the bounded
//! eviction rule, and advice rendering grounded in a catalog.

use super::*;
use crate::IndexKind;
use crate::table::{TableIndex, TableSpec};

fn cat() -> TableCatalog {
    let mut c = TableCatalog::new();
    c.create(TableSpec {
        name: b"ev".to_vec(),
        prefix: b"ev:".to_vec(),
        pk: b"id".to_vec(),
        columns: vec![
            (b"id".to_vec(), ValType::Str),
            (b"at".to_vec(), ValType::I64),
            (b"note".to_vec(), ValType::Str),
        ],
        indexes: vec![TableIndex {
            column: b"at".to_vec(),
            kind: IndexKind::Range,
            values: vec![],
        }],
        orderpaths: vec![],
        window: None,
        autodeclare: 0,
        auto_added: vec![],
    })
    .expect("declare");
    c
}

#[test]
fn families_deduplicate_and_count() {
    // Default = new(): a zero-cap log would panic on first observe.
    let mut log = AdviseLog::default();
    let argv = vec![b"IDX.QUERY".to_vec(), b"ev.at".to_vec()];
    for _ in 0..5 {
        log.observe(b"ev.at", AdviseShape::Range, &argv);
    }
    log.observe(b"ev.note", AdviseShape::Match, &argv);
    let es = log.entries();
    assert_eq!(es.len(), 2);
    assert_eq!((es[0].name.as_slice(), es[0].count), (&b"ev.at"[..], 5));
    assert_eq!(es[1].count, 1);
    assert_eq!(es[0].sample, argv, "first argv kept as the sample");
}

#[test]
fn a_full_log_evicts_its_least_refused_family() {
    let mut log = AdviseLog::with_cap(2);
    let argv: Vec<Vec<u8>> = vec![b"q".to_vec()];
    log.observe(b"a.x", AdviseShape::Range, &argv);
    log.observe(b"a.x", AdviseShape::Range, &argv); // count 2
    log.observe(b"b.y", AdviseShape::Range, &argv); // count 1
    log.observe(b"c.z", AdviseShape::Range, &argv); // evicts b.y
    let names: Vec<_> = log.entries().iter().map(|e| e.name.clone()).collect();
    assert!(names.contains(&b"a.x".to_vec()), "the defended seat survives");
    assert!(names.contains(&b"c.z".to_vec()));
    assert!(!names.contains(&b"b.y".to_vec()), "weakest family made room");
    // An EXISTING family never needs a seat — it just counts.
    log.observe(b"a.x", AdviseShape::Range, &argv);
    assert_eq!(log.entries()[0].count, 3);
}

#[test]
fn apply_auto_declares_each_shape_within_budget() {
    let mut spec = cat().get(b"ev").expect("declared").clone();
    spec.autodeclare = 3;
    let argv: Vec<Vec<u8>> = vec![b"q".to_vec()];
    let mut log = AdviseLog::new();
    log.observe(b"ev.note", AdviseShape::Range, &argv);
    log.observe(b"ev.recent", AdviseShape::Where(vec![b"at".to_vec(), b"note".to_vec()]), &argv);
    log.observe(b"ev.at", AdviseShape::Filter(b"note".to_vec()), &argv);
    log.observe(b"ev.note", AdviseShape::Match, &argv); // advise-only
    log.observe(b"ghost.x", AdviseShape::Range, &argv); // other table
    log.observe(b"ev.nosuch", AdviseShape::Range, &argv); // ungrounded
    let human = spec.sans_auto();
    let applied: Vec<Vec<u8>> =
        log.entries().iter().filter_map(|e| apply_auto(&mut spec, e)).collect();
    assert_eq!(applied.len(), 3, "{applied:?}");
    assert!(spec.indexes.iter().any(|ix| ix.column == b"note"), "Range declared");
    assert!(spec.orderpaths.iter().any(|op| op.name == b"recent"), "Where declared");
    let at = spec.indexes.iter().find(|ix| ix.column == b"at").expect("at");
    assert_eq!(at.values, vec![b"note".to_vec()], "Filter VALUES added");
    assert_eq!(spec.auto_added.len(), 3);
    // The budget is spent — a fourth family is refused.
    let mut extra = AdviseLog::new();
    extra.observe(b"ev.id", AdviseShape::Range, &argv);
    assert!(apply_auto(&mut spec, extra.entries()[0]).is_none(), "budget spent");
    // The human declaration is recoverable exactly.
    assert_eq!(spec.sans_auto(), human, "sans_auto strips every auto addition");
    // And validate + the sidecar round-trip both hold with auto state.
    spec.validate().expect("auto-grown spec validates");
    let mut c = TableCatalog::new();
    c.create(spec.clone()).expect("declare grown");
    let back = TableCatalog::from_sidecar(&c.to_sidecar()).expect("parse");
    assert_eq!(back.get(b"ev"), Some(&spec), "sidecar round-trips auto state");
}

#[test]
fn narrow_advice_needs_a_window_an_observation_and_a_bucket_of_margin() {
    let mut spec = cat().get(b"ev").expect("declared").clone();
    assert_eq!(narrow_advice(&spec, 100), None, "windowless table never advises");
    spec.window = Some(crate::WindowSpec { column: b"at".to_vec(), span: 100, bucket: 10 });
    assert_eq!(narrow_advice(&spec, i64::MAX), None, "unobserved path stays quiet");
    assert_eq!(narrow_advice(&spec, 0), None, "a query touched the boundary");
    assert_eq!(narrow_advice(&spec, -5), None, "a query probed the cold side");
    assert_eq!(narrow_advice(&spec, 7), None, "margin under one bucket");
    let a = narrow_advice(&spec, 37).expect("bucket-aligned narrowing");
    assert_eq!(
        a,
        "WINDOW at SPAN 100 — every observed query kept a margin of 37; SPAN 70 still serves them"
    );
    // A margin at (or past) the whole span still leaves one bucket.
    let a = narrow_advice(&spec, 100).expect("floor at one bucket");
    assert!(a.ends_with("SPAN 10 still serves them"), "{a}");
}

#[test]
fn advice_renders_each_shape_and_refuses_ungrounded_names() {
    let cat = cat();
    let argv: Vec<Vec<u8>> = vec![b"q".to_vec()];
    let mut log = AdviseLog::new();
    log.observe(b"ev.at", AdviseShape::Range, &argv);
    log.observe(b"ev.recent", AdviseShape::Where(vec![b"at".to_vec(), b"note".to_vec()]), &argv);
    log.observe(b"ev.note", AdviseShape::Match, &argv);
    log.observe(b"ev.at", AdviseShape::Filter(b"note".to_vec()), &argv);
    log.observe(b"ghost.col", AdviseShape::Range, &argv);
    log.observe(b"ev.nosuch", AdviseShape::Range, &argv);
    let texts: Vec<String> = log.entries().iter().filter_map(|e| advice_of(e, &cat)).collect();
    assert_eq!(texts.len(), 4, "ungrounded names render nothing: {texts:?}");
    assert!(texts.iter().any(|t| t.contains("INDEX at range")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("ORDERPATH recent ON at THEN note")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("KIND text")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("VALUES note")), "{texts:?}");
}
