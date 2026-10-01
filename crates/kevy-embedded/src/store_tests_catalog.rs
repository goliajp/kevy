//! What the typed catalog calls refuse, and that a refused declaration
//! installs nothing: indexes whose spec the catalog rules out, tables
//! that do not compile, collide with an index or overflow the catalog.

use crate::{
    AnnSpec, Config, IndexKind, IndexValType, KevyError, Store, TableSpec, TokenPositions,
};

fn store() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

/// The refusal's text: the store's own checks answer `InvalidInput`, a
/// spec the index builder rules out an `InvalidInput` I/O error.
fn invalid<T: std::fmt::Debug>(r: crate::KevyResult<T>) -> String {
    match r {
        Err(KevyError::InvalidInput(m)) => m,
        Err(KevyError::Io(e)) if e.kind() == std::io::ErrorKind::InvalidInput => e.to_string(),
        other => panic!("expected an invalid-input refusal, got {other:?}"),
    }
}

fn table(decl: &str) -> TableSpec {
    let argv: Vec<&[u8]> = decl.split(' ').map(str::as_bytes).collect();
    kevy_index::parse_table_declare(&argv).unwrap()
}

fn index_names(s: &Store) -> Vec<Vec<u8>> {
    s.idx_list().into_iter().map(|(name, _, _)| name).collect()
}

#[test]
fn an_index_the_catalog_rules_out_is_refused_and_installs_nothing() {
    let s = store();
    let (i64t, vect) = (IndexValType::I64, IndexValType::Vector);
    assert_eq!(invalid(s.idx_create(b"a", b"", b"f", i64t, IndexKind::Range)), "empty prefix");
    assert!(invalid(s.idx_create(b"a", b"p:", b"f", vect, IndexKind::Range)).contains("vector"));
    let values: &[(&[u8], IndexValType)] = &[(b"v", i64t)];
    let with_values =
        |prefix: &[u8], kind| s.idx_create_with_values(b"b", prefix, b"f", i64t, kind, values);
    assert_eq!(invalid(with_values(b"", IndexKind::Range)), "empty prefix");
    assert!(!invalid(with_values(b"p:", IndexKind::Agg)).is_empty());
    let fields: &[(&[u8], f32)] = &[(b"title", 1.0)];
    let text =
        |prefix: &[u8], fields| s.idx_create_text(b"c", prefix, fields, TokenPositions::Omit, &[]);
    assert_eq!(invalid(text(b"", fields)), "empty prefix");
    assert_eq!(invalid(text(b"p:", &[])), "a text index needs at least one field");
    assert!(index_names(&s).is_empty(), "a refused declaration installed an index");
}

#[test]
fn an_ann_index_keeps_the_graph_parameters_it_was_given() {
    let s = store();
    s.idx_create_ann(b"g", b"g:", b"v", AnnSpec::new(2).with_distance(1).with_m(8).with_ef(64))
        .unwrap();
    let spec = s.idx_spec(b"g").unwrap();
    let ann = spec.ann().unwrap();
    assert_eq!((ann.dim, ann.m, ann.ef), (2, 8, 64));
}

#[test]
fn a_table_that_does_not_compile_is_refused_and_the_old_one_stands() {
    let s = store();
    let users =
        "TABLE.DECLARE users PREFIX users: PK id COLUMN id i64 COLUMN email str INDEX email unique";
    let mut broken = table(users);
    broken.columns.clear();
    assert!(!invalid(s.table_declare(broken.clone())).is_empty());
    assert!(s.table_list().is_empty());

    s.table_declare(table(users)).unwrap();
    assert!(!invalid(s.table_replace(broken)).is_empty());
    assert_eq!(s.table_list()[0].name, b"users");
    assert_eq!(index_names(&s), [b"users.email".to_vec()], "the old table kept its index");
}

#[test]
fn a_table_whose_index_name_is_taken_installs_nothing() {
    let s = store();
    s.idx_create(b"users.email", b"users:", b"email", IndexValType::Str, IndexKind::Unique)
        .unwrap();
    let users =
        "TABLE.DECLARE users PREFIX users: PK id COLUMN id i64 COLUMN email str INDEX email unique";
    assert!(invalid(s.table_declare(table(users))).contains("exists"));
    assert!(s.table_list().is_empty());
}

#[test]
fn a_full_table_catalog_refuses_one_more() {
    let s = store();
    for i in 0..kevy_index::MAX_TABLES {
        s.table_declare(table(&format!("TABLE.DECLARE t{i} PREFIX t{i}: PK id COLUMN id i64")))
            .unwrap();
    }
    let extra = table("TABLE.DECLARE extra PREFIX x: PK id COLUMN id i64");
    assert!(!invalid(s.table_declare(extra)).is_empty());
    assert_eq!(s.table_list().len(), kevy_index::MAX_TABLES);
}
