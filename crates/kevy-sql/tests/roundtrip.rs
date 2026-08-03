//! The drift net: every `TABLE.DECLARE` argv this compiler emits must
//! parse through kevy-index's OWN wire parser (`parse_table_declare`)
//! and compile through `compile_table` — the engine is the authority on
//! the grammar, and this test is where a divergence fails loudly.
//! (kevy-index is a dev-dependency only; the runtime crate stays
//! pure-std 0-dep.)

use kevy_sql::compile;

const SHOP: &str = r"
CREATE TABLE users (
  id bigserial PRIMARY KEY, email text, name text, plan text);
CREATE UNIQUE INDEX ON users (email);
CREATE TABLE orders (
  id bigserial PRIMARY KEY, user_id bigint, status text,
  total numeric(10,2), created_at bigint);
CREATE INDEX ON orders (status) INCLUDE (total, created_at);
CREATE INDEX ON orders (user_id, created_at DESC);
CREATE TABLE order_items (
  id bigserial PRIMARY KEY, order_id bigint, sku text, qty int);
CREATE INDEX ON order_items (order_id);
";

/// Everything the grammar can emit, in one declaration.
const KITCHEN_SINK: &str = r#"
CREATE TABLE "Sink" (
  id bigint PRIMARY KEY, a bigint, b real, c text, d timestamptz,
  e jsonb, f boolean, UNIQUE (c));
CREATE INDEX ON "Sink" (a) INCLUDE (b, c);
CREATE INDEX ON "Sink" (b);
CREATE INDEX deep ON "Sink" (a, b, c, d, e, f);
CREATE INDEX two ON "Sink" (c, a);
"#;

fn assert_roundtrips(sql: &str, expect_tables: usize) {
    let c = compile(sql).unwrap();
    let declares: Vec<&Vec<String>> =
        c.commands.iter().filter(|a| a[0] == "TABLE.DECLARE").collect();
    assert_eq!(declares.len(), expect_tables);
    for argv in declares {
        let raw: Vec<&[u8]> = argv.iter().map(|s| s.as_bytes()).collect();
        let spec = kevy_index::parse_table_declare(&raw)
            .unwrap_or_else(|e| panic!("engine refused compiled declare: {e}\nargv: {argv:?}"));
        // The compiled access paths derive without panicking and carry
        // the dotted `<table>.<suffix>` names.
        let specs = kevy_index::compile_table(&spec).expect("valid spec compiles");
        assert_eq!(specs.len(), spec.indexes.len() + spec.orderpaths.len());
        for s in &specs {
            let name = String::from_utf8(s.name.clone()).unwrap();
            let table = String::from_utf8(spec.name.clone()).unwrap();
            assert!(name.starts_with(&format!("{table}.")), "{name}");
        }
    }
}

#[test]
fn shop_declares_roundtrip_through_the_engine_parser() {
    assert_roundtrips(SHOP, 3);
}

#[test]
fn kitchen_sink_roundtrips() {
    assert_roundtrips(KITCHEN_SINK, 1);
}

#[test]
fn compiled_composite_matches_engine_where_bounds() {
    // The card's WHERE clause must be computable by the engine's own
    // composite_bounds over the compiled orderpath — leading-prefix in
    // the declared order.
    let c = compile(
        "CREATE TABLE t (id bigint PRIMARY KEY, a text, b bigint);
         CREATE INDEX p ON t (a, b DESC);
         CREATE VIEW v AS SELECT * FROM t WHERE a = $1 ORDER BY b DESC;",
    )
    .unwrap();
    let raw: Vec<&[u8]> = c.commands[0].iter().map(|s| s.as_bytes()).collect();
    let spec = kevy_index::parse_table_declare(&raw).unwrap();
    let specs = kevy_index::compile_table(&spec).expect("valid spec compiles");
    let comp = specs.iter().find(|s| s.name == b"t.p").expect("orderpath spec");
    let cols = comp.composite.as_ref().expect("composite");
    let w = kevy_index::WhereClause { eqs: vec![(b"a".to_vec(), b"x".to_vec())], range: None };
    kevy_index::composite_bounds(cols, &w, 0).expect("engine accepts the compiled prefix");
}
