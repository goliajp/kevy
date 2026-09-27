//! `select_card` and `table_ddl` against declarations as a server
//! describes them.

use super::*;

fn words(line: &str) -> Vec<String> {
    line.split(' ').map(String::from).collect()
}

const ORDERS: &str = "TABLE.DECLARE orders PREFIX orders: PK id COLUMN id i64 COLUMN user_id i64 COLUMN total f64 COLUMN status str INDEX user_id range VALUES status total INDEX status unique ORDERPATH by_user_total ON user_id THEN total DESC";

#[test]
fn the_sql_form_compiles_back_to_the_same_declaration() {
    let ddl = table_ddl(&words(ORDERS)).expect("renders");
    assert_eq!(
        ddl,
        "CREATE TABLE orders (\n    id bigint PRIMARY KEY,\n    user_id bigint,\n    total double precision,\n    status text\n);\n\
         CREATE INDEX ON orders (user_id) INCLUDE (status, total);\n\
         CREATE UNIQUE INDEX ON orders (status);\n\
         CREATE INDEX by_user_total ON orders (user_id, total DESC);\n"
    );
    let back = crate::compile(&ddl).expect("the rendered SQL compiles");
    assert_eq!(back.commands, vec![words(ORDERS)]);
}

#[test]
fn what_sql_cannot_say_is_written_as_a_comment() {
    let decl = "TABLE.DECLARE Ev PREFIX e: PK id COLUMN id str COLUMN at i64 ORDERPATH recent ON at DESC ORDERPATH first ON at WINDOW at SPAN 60 BUCKET 10 AUTODECLARE 3";
    let ddl = table_ddl(&words(decl)).expect("renders");
    assert_eq!(
        ddl,
        "CREATE TABLE \"Ev\" (\n    id text PRIMARY KEY,\n    at bigint\n);\n\
         CREATE INDEX recent ON \"Ev\" (at DESC);\n\
         -- not carried by SQL: PREFIX e:\n\
         -- not carried by SQL: ORDERPATH first ON at\n\
         -- not carried by SQL: WINDOW at SPAN 60 BUCKET 10\n\
         -- not carried by SQL: AUTODECLARE 3\n"
    );
    let quote = "TABLE.DECLARE a\"b PREFIX a: PK id COLUMN id str";
    assert!(table_ddl(&words(quote)).unwrap_err().contains("no SQL spelling"));
}

#[test]
fn a_descending_index_is_an_order_path_that_serves_order_by_desc() {
    let schema = "CREATE TABLE ev (id bigint PRIMARY KEY, at bigint);\nCREATE INDEX ON ev (at);\n";
    let refused = crate::plan(&format!(
        "{schema}CREATE VIEW latest AS SELECT id FROM ev WHERE at >= $1 ORDER BY at DESC LIMIT 10;"
    ))
    .expect("parses");
    let crate::Served::No { reason } = &refused.queries[0].served else {
        panic!("no path reads backwards yet")
    };
    assert!(
        reason.ends_with("ORDER BY at DESC reads the index on at backwards, which a Range index does not do \u{2014} declare CREATE INDEX at_desc ON ev (at DESC)"),
        "{reason}"
    );
    let declared = format!("{schema}CREATE INDEX at_desc ON ev (at DESC);\n");
    let compiled = crate::compile(&declared).expect("a DESC index compiles");
    assert_eq!(
        compiled.commands[0].join(" "),
        "TABLE.DECLARE ev PREFIX ev: PK id COLUMN id i64 COLUMN at i64 INDEX at range ORDERPATH at_desc ON at DESC"
    );
    let card = select_card(
        &compiled.commands,
        "SELECT id FROM ev WHERE at >= 15 ORDER BY at DESC LIMIT 2",
    )
    .expect("served by the order path");
    assert_eq!(
        card.argv[..3],
        ["IDX.QUERY".to_string(), "ev.at_desc".to_string(), "WHERE".to_string()]
    );
    let ddl = table_ddl(&compiled.commands[0]).expect("renders");
    assert_eq!(crate::compile(&ddl).expect("reads back").commands, compiled.commands);
    let unnamed =
        crate::compile(&format!("{schema}CREATE INDEX ON ev (at DESC);\n")).expect("compiles");
    assert!(unnamed.commands[0].join(" ").ends_with("ORDERPATH at_desc ON at DESC"));
}

#[test]
fn a_select_becomes_the_index_query_that_answers_it() {
    let card = select_card(
        &[words(ORDERS)],
        "SELECT id, total FROM orders WHERE user_id = 7 ORDER BY total DESC LIMIT 5",
    )
    .expect("served by the user_id index");
    // Every predicate is on user_id, which has its own index: path 2
    // (direct drive) is chosen before the order path, and the sort rides
    // the index's stored `total`.
    assert_eq!(
        card.argv,
        words("IDX.QUERY orders.user_id EQ 7 SORT total DESC LIMIT 5 FIELDS id total")
    );
    assert!(card.params.is_empty());
}

#[test]
fn a_select_no_path_serves_is_refused_like_sql_plan_refuses_it() {
    let decls = [words(ORDERS)];
    let err = select_card(&decls, "SELECT * FROM orders WHERE total > 3").unwrap_err();
    let schema = crate::table_ddl(&decls[0]).unwrap();
    let plan = crate::plan(&format!(
        "{schema}CREATE VIEW select AS SELECT * FROM orders WHERE total > 3;"
    ))
    .expect("the schema parses");
    let crate::Served::No { reason } = &plan.queries[0].served else { panic!("plan serves it") };
    assert_eq!(&err.message, reason);
}

#[test]
fn a_select_is_one_literal_query_over_a_declared_table() {
    let decls = [words(ORDERS)];
    let unknown = select_card(&decls, "SELECT * FROM carts WHERE id = 1").unwrap_err();
    assert!(unknown.message.contains("FROM unknown table 'carts'"), "{unknown}");
    let param = select_card(&decls, "SELECT * FROM orders WHERE user_id = $1").unwrap_err();
    assert!(param.message.contains("$1 is a query-card parameter"), "{param}");
    let two = select_card(&decls, "SELECT * FROM orders WHERE id = 1; SELECT 1").unwrap_err();
    assert!(two.message.contains("one statement"), "{two}");
    let not_select = select_card(&decls, "DELETE FROM orders").unwrap_err();
    assert!(not_select.message.contains("expected SELECT"), "{not_select}");
}

#[test]
fn a_declaration_that_does_not_read_is_refused_by_name() {
    let refused = |line: &str| table_ddl(&words(line)).unwrap_err();
    assert!(refused("TABLE.LIST").starts_with("not a TABLE.DECLARE declaration"));
    let head = "TABLE.DECLARE t PREFIX t: PK id";
    assert_eq!(refused(&format!("{head} COLUMN id str COLUMN x")), "COLUMN is cut short");
    assert_eq!(
        refused(&format!("{head} COLUMN id bool x")),
        "column type 'bool' is not i64|f64|str"
    );
    assert_eq!(refused(&format!("{head} COLUMN id str INDEX id")), "INDEX is cut short");
    assert_eq!(
        refused(&format!("{head} COLUMN id str ORDERPATH p id")),
        "ORDERPATH needs <name> ON <col>"
    );
    assert_eq!(refused(&format!("{head} COLUMN id str WINDOW id SPAN")), "WINDOW is cut short");
    assert_eq!(refused(&format!("{head} COLUMN id str EXTRA x")), "unknown clause 'EXTRA'");
    let err = select_card(&[words("TABLE.DECLARE t")], "SELECT * FROM t WHERE id = 1").unwrap_err();
    assert!(err.message.starts_with("not a TABLE.DECLARE declaration"), "{err}");
}

#[test]
fn a_name_sql_cannot_spell_is_refused_wherever_it_appears() {
    let base = "TABLE.DECLARE t PREFIX t: PK id COLUMN id str COLUMN a str COLUMN b str";
    for tail in [
        "COLUMN q\"x str",
        "INDEX a range VALUES q\"x",
        "ORDERPATH p\"q ON a THEN b",
        "ORDERPATH p ON a THEN q\"x",
    ] {
        let err = table_ddl(&words(&format!("{base} {tail}"))).unwrap_err();
        assert!(err.contains("no SQL spelling"), "{tail}: {err}");
    }
    let quoted = table_ddl(&words("TABLE.DECLARE T PREFIX T: PK Id COLUMN Id str INDEX Id unique"))
        .expect("renders");
    assert_eq!(
        quoted,
        "CREATE TABLE \"T\" (\n    \"Id\" text PRIMARY KEY\n);\nCREATE UNIQUE INDEX ON \"T\" (\"Id\");\n"
    );
}

#[test]
fn a_select_that_does_not_lex_or_parse_says_where() {
    let decls = [words(ORDERS)];
    let lexed = select_card(&decls, "SELECT * FROM orders WHERE status = 'open").unwrap_err();
    assert!(lexed.message.contains("unterminated"), "{lexed}");
    let parsed = select_card(&decls, "SELECT * FROM orders WHERE").unwrap_err();
    assert_eq!((parsed.line, parsed.col > 1), (1, true), "{parsed}");
}
