//! What an automatic declaration adds, and each case where it adds
//! nothing. A `#[path]` child of `advise`.

use kevy_index::{AdviseEntry, AdviseShape, IndexKind, IndexSpec, ValType};

use super::autodeclared;
use crate::index_runtime::test_shard::Shard;

const TABLE: &str = "TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 COLUMN a i64 COLUMN b i64 \
    COLUMN note str INDEX a range AUTODECLARE 4";

fn entry(name: &str, shape: AdviseShape) -> AdviseEntry {
    AdviseEntry::new(name.as_bytes().to_vec(), shape, kevy_index::AUTODECLARE_AFTER)
}

fn declared(s: &Shard, table: &str, e: &AdviseEntry) -> Option<crate::state::CatalogChange> {
    autodeclared(&s.cmds.state().catalog_base(), table.as_bytes(), e)
}

#[test]
fn a_filter_refused_on_a_path_adds_the_field_to_its_stored_values() {
    let mut s = Shard::new();
    s.ok(TABLE);
    let change = declared(&s, "t", &entry("t.a", AdviseShape::Filter(b"note".to_vec()))).unwrap();
    let icat = change.index.unwrap();
    let (spec, _) = icat.get(b"t.a").unwrap();
    assert_eq!(
        spec.values().iter().map(|v| v.name.clone()).collect::<Vec<_>>(),
        [b"note".to_vec()]
    );
}

#[test]
fn nothing_is_declared_for_a_table_that_is_not_there_or_did_not_opt_in() {
    let mut s = Shard::new();
    let range = entry("t.b", AdviseShape::Range);
    assert!(declared(&s, "t", &range).is_none(), "no table catalog");
    s.ok(TABLE);
    assert!(declared(&s, "nosuch", &range).is_none());
    s.ok("TABLE.DECLARE plain PREFIX p: PK id COLUMN id i64 COLUMN b i64");
    assert!(declared(&s, "plain", &entry("plain.b", AdviseShape::Range)).is_none());
}

#[test]
fn a_path_whose_declaration_would_not_compile_is_not_declared() {
    let mut s = Shard::new();
    s.ok(TABLE);
    // an ORDERPATH named like the indexed column a collides with it
    let collides = entry("t.a", AdviseShape::Where(vec![b"b".to_vec()]));
    assert!(declared(&s, "t", &collides).is_none());
}

#[test]
fn a_path_the_full_index_catalog_has_no_room_for_is_not_declared() {
    let mut s = Shard::new();
    s.ok(TABLE);
    let mut icat = (*s.cmds.state().catalogs.index().unwrap()).clone();
    // filled until the catalog refuses one more
    for i in 0.. {
        let spec = IndexSpec::builder(format!("x{i}"), "x:", IndexKind::Range, ValType::I64)
            .with_field("v")
            .build()
            .unwrap();
        if icat.create(spec).is_err() {
            break;
        }
    }
    s.cmds.state().install_index_catalog(icat);
    assert!(declared(&s, "t", &entry("t.b", AdviseShape::Range)).is_none());
}
