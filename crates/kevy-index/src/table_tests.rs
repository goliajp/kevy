//! TableSpec / TableCatalog / compile / wire-grammar tests.

use super::*;
use crate::catalog::{IndexKind, ValType};
use crate::table_wire::{TABLE_DECLARE_USAGE, parse_table_declare};

fn declare(parts: &[&str]) -> Result<TableSpec, String> {
    let argv: Vec<&[u8]> = parts.iter().map(|s| s.as_bytes()).collect();
    parse_table_declare(&argv)
}

const USER: &[&str] = &[
    "TABLE.DECLARE", "user", "PREFIX", "user:", "PK", "id",
    "COLUMN", "id", "str", "COLUMN", "name", "str", "COLUMN", "age", "i64",
    "COLUMN", "dept", "str",
    "INDEX", "age", "RANGE", "VALUES", "dept", "name",
    "INDEX", "dept", "UNIQUE",
    "ORDERPATH", "recent_by_dept", "ON", "dept", "THEN", "age", "DESC",
];

#[test]
fn full_declare_parses_and_compiles() {
    let spec = declare(USER).expect("parses");
    assert_eq!(spec.columns.len(), 4);
    assert_eq!(spec.indexes.len(), 2);
    assert_eq!(spec.orderpaths.len(), 1);
    let compiled = compile_table(&spec).expect("valid spec compiles");
    assert_eq!(compiled.len(), 3);
    assert_eq!(compiled[0].name, b"user.age".to_vec());
    assert_eq!(compiled[0].ty, ValType::I64);
    assert_eq!(compiled[0].kind, IndexKind::Range);
    assert_eq!(compiled[0].values.len(), 2);
    assert_eq!(compiled[0].values[0].ty, ValType::Str, "VALUES typed from column decls");
    assert_eq!(compiled[1].name, b"user.dept".to_vec());
    assert_eq!(compiled[1].kind, IndexKind::Unique);
    let op = &compiled[2];
    assert_eq!(op.name, b"user.recent_by_dept".to_vec());
    assert_eq!(op.ty, ValType::Str);
    assert_eq!(op.kind, IndexKind::Range);
    let cols = op.composite.as_ref().expect("composite");
    assert_eq!(cols.len(), 2);
    assert_eq!((cols[0].name.as_slice(), cols[0].ty, cols[0].desc), (b"dept".as_slice(), ValType::Str, false));
    assert_eq!((cols[1].name.as_slice(), cols[1].ty, cols[1].desc), (b"age".as_slice(), ValType::I64, true));
    // Every compiled spec is admissible as-is.
    let mut cat = crate::Catalog::new();
    for s in compiled {
        cat.create(s).expect("compiled specs admit");
    }
}

#[test]
fn every_grammar_refusal_is_named() {
    let e = |parts: &[&str]| declare(parts).unwrap_err();
    assert_eq!(e(&["TABLE.DECLARE", "t", "PREFIX", "p:"]), TABLE_DECLARE_USAGE);
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "uuid"]),
        "ERR COLUMN type must be i64|f64|str"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id",
            "COLUMN", "id", "str", "COLUMN", "id", "i64"]),
        "ERR duplicate COLUMN 'id'"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "nope", "COLUMN", "id", "str"]),
        "ERR PK column 'nope' is not declared (add COLUMN nope ...)"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "str",
            "INDEX", "ghost", "RANGE"]),
        "ERR INDEX names unknown column 'ghost'"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "str",
            "INDEX", "id", "agg"]),
        "ERR INDEX kind must be range|unique"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "str",
            "INDEX", "id", "RANGE", "INDEX", "id", "UNIQUE"]),
        "ERR duplicate INDEX on column 'id'"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "str",
            "INDEX", "id", "RANGE", "VALUES", "ghost"]),
        "ERR VALUES names unknown column 'ghost'"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "str",
            "INDEX", "id", "RANGE", "VALUES", "INDEX", "id", "UNIQUE"]),
        "ERR VALUES needs at least one column"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "str",
            "ORDERPATH", "op", "ON", "ghost"]),
        "ERR ORDERPATH 'op' names unknown column 'ghost'"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "str",
            "ORDERPATH", "op", "ON", "id", "ORDERPATH", "op", "ON", "id"]),
        "ERR duplicate ORDERPATH 'op'"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "str",
            "ORDERPATH", "op", "BY", "id"]),
        "ERR ORDERPATH needs ON <col>"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "", "PK", "id", "COLUMN", "id", "str"]),
        "ERR PREFIX must be non-empty"
    );
    assert_eq!(
        e(&["TABLE.DECLARE", "t", "PREFIX", "p:", "PK", "id", "COLUMN", "id", "str",
            "INDEX", "id", "RANGE", "ORDERPATH", "id", "ON", "id"]),
        "ERR ORDERPATH 'id' collides with INDEX 'id'"
    );
}

#[test]
fn catalog_lifecycle_and_caps() {
    let spec = declare(USER).expect("parses");
    let mut c = TableCatalog::new();
    c.create(spec.clone()).expect("creates");
    assert_eq!(c.create(spec.clone()).unwrap_err(), "ERR table already exists");
    assert!(c.get(b"user").is_some());
    assert_eq!(c.len(), 1);
    assert!(c.drop_table(b"user"));
    assert!(!c.drop_table(b"user"));
    assert!(c.is_empty());
    for i in 0..MAX_TABLES {
        let mut s = spec.clone();
        s.name = format!("t{i}").into_bytes();
        c.create(s).expect("under cap");
    }
    let mut over = spec.clone();
    over.name = b"over".to_vec();
    assert_eq!(c.create(over).unwrap_err(), "ERR table limit reached (64)");
}

#[test]
fn sidecar_round_trips_with_hostile_names() {
    let mut spec = declare(USER).expect("parses");
    spec.name = b"we\tird,ta:ble".to_vec();
    spec.prefix = b"pre\nfix%:".to_vec();
    let mut c = TableCatalog::new();
    c.create(spec.clone()).expect("creates");
    let text = c.to_sidecar();
    let c2 = TableCatalog::from_sidecar(&text).expect("loads");
    assert_eq!(c2.get(&spec.name), Some(&spec), "byte-exact round trip");
    assert!(TableCatalog::from_sidecar("bogus").is_none());
    assert!(
        TableCatalog::from_sidecar("kevy-table-catalog v1\ngarbage line").is_none(),
        "a malformed line refuses the whole load"
    );
}

mod window {
    use super::super::*;
    use crate::table_wire::parse_table_declare;

    fn declare(extra: &str) -> Result<TableSpec, String> {
        let base = "TABLE.DECLARE ev PREFIX ev: PK id COLUMN id str COLUMN at i64 COLUMN note str";
        let full = format!("{base} {extra}");
        let argv: Vec<&[u8]> = full.split(' ').map(str::as_bytes).collect();
        parse_table_declare(&argv)
    }

    #[test]
    fn window_parses_with_an_index_access_path() {
        let spec = declare("INDEX at range WINDOW at SPAN 90 BUCKET 1").expect("valid");
        let w = spec.window.expect("window kept");
        assert_eq!((w.column.as_slice(), w.span, w.bucket), (b"at".as_slice(), 90, 1));
    }

    #[test]
    fn window_accepts_a_leading_ascending_orderpath() {
        let spec = declare("ORDERPATH recent ON at THEN id WINDOW at SPAN 90 BUCKET 30");
        assert!(spec.is_ok(), "{spec:?}");
    }

    #[test]
    fn window_refusals_are_named() {
        for (extra, needle) in [
            ("WINDOW at SPAN 90 BUCKET 1", "needs an access path"),
            ("INDEX at range WINDOW ghost SPAN 9 BUCKET 1", "unknown column"),
            ("INDEX note range WINDOW note SPAN 9 BUCKET 1", "must be i64"),
            ("INDEX at range WINDOW at SPAN 0 BUCKET 1", "must be positive"),
            ("INDEX at range WINDOW at SPAN 9 BUCKET -1", "must be positive"),
            ("INDEX at range WINDOW at SPAN 9 BUCKET 10", "must not exceed SPAN"),
            ("INDEX at range WINDOW at SPAN x BUCKET 1", "must be an integer"),
            (
                "ORDERPATH recent ON at DESC WINDOW at SPAN 9 BUCKET 1",
                "needs an access path",
            ),
            (
                "INDEX at range WINDOW at SPAN 9 BUCKET 1 WINDOW at SPAN 9 BUCKET 1",
                "duplicate WINDOW",
            ),
        ] {
            let err = declare(extra).expect_err(extra);
            assert!(err.contains(needle), "{extra}: {err}");
        }
    }

    #[test]
    fn sidecar_round_trips_the_window_and_stays_bytewise_stable_without_one() {
        let mut cat = TableCatalog::new();
        cat.create(declare("INDEX at range WINDOW at SPAN 90 BUCKET 1").unwrap()).unwrap();
        cat.create(declare("INDEX at range").map(|mut s| { s.name = b"plain".to_vec(); s }).unwrap())
            .unwrap();
        let text = cat.to_sidecar();
        let back = TableCatalog::from_sidecar(&text).expect("parses");
        assert_eq!(back.get(b"ev").unwrap().window, cat.get(b"ev").unwrap().window);
        assert_eq!(back.get(b"plain").unwrap().window, None);
        // The windowless line keeps the six-field shape older readers know.
        let plain_line = text.lines().find(|l| l.starts_with("plain")).unwrap();
        assert_eq!(plain_line.split('\t').count(), 6);
        let ev_line = text.lines().find(|l| l.starts_with("ev")).unwrap();
        assert_eq!(ev_line.split('\t').count(), 7);
    }
}
