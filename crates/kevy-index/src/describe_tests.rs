//! Describe replies and the declarations they carry.

use super::*;
use crate::catalog::{AnnSpec, FieldSpec, ValueSpec};
use crate::table::TableIndex;
use crate::table_wire::parse_table_declare;
use crate::value::IndexValue;
use crate::view::{Leaf, Tree, ViewMode, ViewSpec};

fn words(ws: &[&str]) -> Vec<Vec<u8>> {
    ws.iter().map(|w| w.as_bytes().to_vec()).collect()
}

fn declare(ws: &[&str]) -> TableSpec {
    let argv = words(ws);
    let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    parse_table_declare(&refs).expect("the test declarations are valid")
}

fn field<'a>(d: &'a Described, label: &str) -> &'a Described {
    let Described::Array(items) = d else { panic!("a describe reply is an array") };
    let at = items.iter().position(|i| *i == Described::Bulk(label.as_bytes().to_vec()));
    &items[at.expect("the label is present") + 1]
}

const FULL: &[&str] = &[
    "TABLE.DECLARE",
    "order",
    "PREFIX",
    "order:",
    "PK",
    "id",
    "COLUMN",
    "id",
    "str",
    "COLUMN",
    "total",
    "f64",
    "COLUMN",
    "at",
    "i64",
    "COLUMN",
    "who",
    "str",
    "INDEX",
    "at",
    "range",
    "VALUES",
    "total",
    "who",
    "INDEX",
    "id",
    "unique",
    "ORDERPATH",
    "recent",
    "ON",
    "who",
    "THEN",
    "at",
    "DESC",
    "WINDOW",
    "at",
    "SPAN",
    "86400",
    "BUCKET",
    "3600",
    "AUTODECLARE",
    "2",
];

#[test]
fn a_table_declaration_parses_back_to_the_same_spec() {
    let spec = declare(FULL);
    let again = table_declaration(&spec);
    assert_eq!(again, words(FULL));
    let refs: Vec<&[u8]> = again.iter().map(Vec::as_slice).collect();
    assert_eq!(parse_table_declare(&refs).expect("it parses"), spec);
}

#[test]
fn a_table_declaration_leaves_out_what_the_auto_loop_added() {
    let mut spec = declare(FULL);
    spec.indexes.push(TableIndex {
        column: b"who".to_vec(),
        kind: IndexKind::Range,
        values: vec![],
    });
    spec.auto_added.push(b"order.who".to_vec());
    let d = describe_table(&spec);
    assert_eq!(field(&d, "declaration"), &argv(words(FULL)));
    let Described::Array(indexes) = field(&d, "indexes") else { panic!("indexes is an array") };
    assert_eq!(indexes.len(), 3);
    assert_eq!(field(&indexes[2], "auto"), &b("1"));
    assert_eq!(field(&indexes[0], "auto"), &b("0"));
}

#[test]
fn a_compiled_index_names_its_table_and_has_no_declaration() {
    let spec = declare(FULL);
    let tables = [spec.clone()];
    let compiled = compile_table(&spec).expect("compiles");
    for c in &compiled {
        let d = describe_index(c, &tables);
        assert_eq!(field(&d, "table"), &b("order"));
        assert_eq!(field(&d, "declaration"), &b("-"));
    }
    let recent = describe_index(&compiled[2], []);
    let Described::Array(cols) = field(&recent, "composite") else {
        panic!("an orderpath compiles to a composite")
    };
    assert_eq!(cols[1], Described::Array(vec![b("at"), b("i64"), b("desc")]));
}

#[test]
fn a_weighted_text_index_is_spelled_with_fields_and_weights() {
    let mut s = IndexSpec::single_field(
        b"doc.body".to_vec(),
        b"doc:".to_vec(),
        b"title".to_vec(),
        ValType::Str,
        IndexKind::Text,
    );
    s.fields = vec![FieldSpec { name: b"title".to_vec(), weight: 2.5 }, FieldSpec::new("body")];
    s.with_positions = true;
    s.values = vec![ValueSpec { name: b"year".to_vec(), ty: ValType::I64 }, ValueSpec::new("tag")];
    s.max_bytes = 1024;
    assert_eq!(
        index_declaration(&s),
        words(&[
            "IDX.CREATE",
            "doc.body",
            "ON",
            "PREFIX",
            "doc:",
            "FIELDS",
            "title",
            "body",
            "WEIGHTS",
            "2.5",
            "1",
            "TYPE",
            "str",
            "KIND",
            "text",
            "WITH",
            "POSITIONS",
            "VALUES",
            "year",
            "tag",
            "TYPES",
            "i64",
            "str",
            "MAXMEM",
            "1024",
        ])
    );
}

#[test]
fn a_single_field_index_is_spelled_with_field() {
    let mut s = IndexSpec::single_field(
        b"emb".to_vec(),
        b"v:".to_vec(),
        b"vec".to_vec(),
        ValType::Vector,
        IndexKind::Ann,
    );
    s.ann = Some(AnnSpec { dim: 4, distance: 1, m: 16, ef: 200 });
    assert_eq!(
        index_declaration(&s),
        words(&[
            "IDX.CREATE",
            "emb",
            "ON",
            "PREFIX",
            "v:",
            "FIELD",
            "vec",
            "TYPE",
            "vector",
            "KIND",
            "ann",
            "DIM",
            "4",
            "DISTANCE",
            "l2",
            "M",
            "16",
            "EF",
            "200",
        ])
    );
}

#[test]
fn a_view_declaration_writes_the_tree_as_create_reads_it() {
    let leaf = |index: &str, min: IndexValue, max: IndexValue| {
        Box::new(Tree::Leaf(Leaf { index: index.as_bytes().to_vec(), min, max }))
    };
    let v = ViewSpec {
        name: b"hot".to_vec(),
        tree: Tree::Diff(
            leaf("age", IndexValue::I64(18), IndexValue::I64(65)),
            leaf("score", IndexValue::F64(0.5), IndexValue::F64(0.5)),
        ),
        order_by: b"age".to_vec(),
        desc: true,
        mode: ViewMode::Materialized { top_k: 10 },
        via: Some(b"user:{key}".to_vec()),
    };
    let expected = words(&[
        "VIEW.CREATE",
        "hot",
        "QUERY",
        "(",
        "DIFF",
        "age",
        "RANGE",
        "18",
        "65",
        "score",
        "EQ",
        "0.5",
        ")",
        "ORDER",
        "BY",
        "age",
        "DESC",
        "MODE",
        "materialized",
        "TOPK",
        "10",
        "VIA",
        "user:{key}",
    ]);
    assert_eq!(view_declaration(&v), expected);
    let d = describe_view(&v);
    assert_eq!(field(&d, "query"), &argv(expected[3..13].to_vec()));
    assert_eq!(field(&d, "topk"), &b("10"));
}
