//! The table registry's admission, sidecar and window questions, and the
//! declaration clauses that refuse on their own.

use super::*;
use crate::{
    CatalogError, GlobalPath, IndexKind, TableError, ValType, parse_table_declare,
    parse_table_declare_partitioned,
};

fn declare(s: &str) -> Result<TableSpec, TableError> {
    parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>())
}

const EVENTS: &str = "TABLE.DECLARE ev PREFIX ev: PK id COLUMN id i64 COLUMN at i64 INDEX at range WINDOW at SPAN 100 BUCKET 10";

#[test]
fn an_edited_spec_is_validated_again_on_compile_and_admission() {
    let mut t = declare(EVENTS).unwrap();
    t.prefix.clear();
    assert_eq!(t.compile(), Err(TableError::EmptyPrefix));
    assert_eq!(TableCatalog::new().create(t), Err(CatalogError::Table(TableError::EmptyPrefix)));
}

#[test]
fn a_sidecar_loads_only_whole_and_well_formed() {
    let mut c = TableCatalog::new();
    c.create(declare(EVENTS).unwrap()).unwrap();
    let text = c.to_sidecar();
    let (header, line) = text.trim_end().split_once('\n').unwrap();
    let spaced = format!("{header}\n\n{line}\n\n");
    assert_eq!(TableCatalog::from_sidecar(&spaced).map(|c| c.len()), Some(1), "blank lines skip");
    assert!(TableCatalog::from_sidecar("").is_none());
    assert!(TableCatalog::from_sidecar("kevy-table-catalog v9\n").is_none());
    assert!(TableCatalog::from_sidecar(&format!("{header}\nnot a table\n")).is_none());
    assert!(TableCatalog::from_sidecar(&format!("{header}\n{line}\n{line}\n")).is_none(), "twice");
}

#[test]
fn only_a_dotted_path_of_a_windowed_table_drives_its_window() {
    let mut c = TableCatalog::new();
    c.create(declare(EVENTS).unwrap()).unwrap();
    c.create(declare("TABLE.DECLARE plain PREFIX p: PK id COLUMN id i64 INDEX id range").unwrap())
        .unwrap();
    assert!(c.is_window_driver(b"ev.at"));
    assert!(!c.is_window_driver(b"ev.id"));
    assert!(!c.is_window_driver(b"evat"), "not a compiled path");
    assert!(!c.is_window_driver(b"gone.at"), "no such table");
    assert!(!c.is_window_driver(b"plain.id"), "no window");
    let text = |name: &str| {
        crate::IndexSpec::builder(name, "ev:", IndexKind::Text, ValType::Str)
            .with_field("note")
            .build()
            .unwrap()
    };
    assert!(c.is_windowed_text(&text("ev.note")));
    assert!(!c.is_windowed_text(&text("evnote")), "not a compiled path");
    assert!(!c.is_windowed_text(&text("plain.note")));
}

#[test]
fn a_clause_after_the_columns_refuses_on_its_own() {
    let partitioned = |s: &str| {
        parse_table_declare_partitioned(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>())
    };
    let head = "TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 COLUMN at i64";
    for (tail, want) in [
        ("INDEX at range GLOBAL SPLIT AT", TableError::SplitAtEmpty),
        ("ORDERPATH p ON at GLOBAL SPLIT AT", TableError::SplitAtEmpty),
        ("ORDERPATH p ON at GLOBAL SPLIT 5", TableError::Usage),
        ("AUTODECLARE many", TableError::AutodeclareNotPositive),
        ("PARTITION at", TableError::Usage),
    ] {
        assert_eq!(partitioned(&format!("{head} {tail}")), Err(want), "{tail}");
    }
    let (_, globals) = partitioned(&format!("{head} ORDERPATH p ON at GLOBAL")).unwrap();
    assert_eq!(globals, [GlobalPath::new("t.p")], "sampled split points");
}
