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
    let decl = "TABLE.DECLARE Ev PREFIX e: PK id COLUMN id str COLUMN at i64 ORDERPATH recent ON at DESC WINDOW at SPAN 60 BUCKET 10 AUTODECLARE 3";
    let ddl = table_ddl(&words(decl)).expect("renders");
    assert_eq!(
        ddl,
        "CREATE TABLE \"Ev\" (\n    id text PRIMARY KEY,\n    at bigint\n);\n\
         -- not carried by SQL: PREFIX e:\n\
         -- not carried by SQL: ORDERPATH recent ON at DESC\n\
         -- not carried by SQL: WINDOW at SPAN 60 BUCKET 10\n\
         -- not carried by SQL: AUTODECLARE 3\n"
    );
    let quote = "TABLE.DECLARE a\"b PREFIX a: PK id COLUMN id str";
    assert!(table_ddl(&words(quote)).unwrap_err().contains("no SQL spelling"));
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
