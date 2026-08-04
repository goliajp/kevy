//! [`super`]'s tests: family dedup and counting, the bounded
//! eviction rule, and advice rendering grounded in a catalog.

use super::*;
use crate::table::{TableIndex, TableSpec};
use crate::IndexKind;

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
    })
    .expect("declare");
    c
}

#[test]
fn families_deduplicate_and_count() {
    let mut log = AdviseLog::new();
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
fn advice_renders_each_shape_and_refuses_ungrounded_names() {
    let cat = cat();
    let argv: Vec<Vec<u8>> = vec![b"q".to_vec()];
    let mut log = AdviseLog::new();
    log.observe(b"ev.at", AdviseShape::Range, &argv);
    log.observe(
        b"ev.recent",
        AdviseShape::Where(vec![b"at".to_vec(), b"note".to_vec()]),
        &argv,
    );
    log.observe(b"ev.note", AdviseShape::Match, &argv);
    log.observe(b"ev.at", AdviseShape::Filter(b"note".to_vec()), &argv);
    log.observe(b"ghost.col", AdviseShape::Range, &argv);
    log.observe(b"ev.nosuch", AdviseShape::Range, &argv);
    let texts: Vec<String> = log
        .entries()
        .iter()
        .filter_map(|e| advice_of(e, &cat))
        .collect();
    assert_eq!(texts.len(), 4, "ungrounded names render nothing: {texts:?}");
    assert!(texts.iter().any(|t| t.contains("INDEX at range")), "{texts:?}");
    assert!(
        texts.iter().any(|t| t.contains("ORDERPATH recent ON at THEN note")),
        "{texts:?}"
    );
    assert!(texts.iter().any(|t| t.contains("KIND text")), "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("VALUES note")), "{texts:?}");
}
