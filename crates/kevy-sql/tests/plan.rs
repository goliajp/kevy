//! Plan semantics: every query gets a verdict, a schema that does not
//! parse still fails, and a refusal keeps naming its fix.

use kevy_sql::{Served, compile, plan};

/// Two views that cannot be served, with a servable one between them.
/// `compile` stops at the first; a plan has to report all three.
const MIXED: &str = r"
CREATE TABLE orders (
  id          bigserial PRIMARY KEY,
  user_id     bigint,
  status      text,
  total       bigint,
  created_at  bigint
);
CREATE INDEX ON orders (status);
CREATE INDEX ON orders (user_id, created_at DESC);

CREATE VIEW by_total AS
  SELECT * FROM orders WHERE total = $1;

CREATE VIEW paid AS
  SELECT * FROM orders WHERE status = 'paid';

CREATE VIEW everything AS
  SELECT * FROM orders;
";

#[test]
fn compile_stops_at_the_first_refusal_and_plan_does_not() {
    // The shape this whole entry point exists for: `compile` answers
    // about one view, `plan` answers about all of them.
    assert!(compile(MIXED).is_err(), "compile must still refuse the file");

    let p = plan(MIXED).expect("the DDL parses, so there is a plan");
    assert_eq!(p.queries.len(), 3, "every view gets a row, servable or not");
    assert_eq!(p.unserved(), 2);

    let names: Vec<&str> = p.queries.iter().map(|q| q.name.as_str()).collect();
    assert_eq!(names, ["by_total", "paid", "everything"], "source order kept");

    assert!(!p.queries[0].served.is_served(), "no path on `total`");
    assert!(p.queries[1].served.is_served(), "orders.status is declared");
    assert!(!p.queries[2].served.is_served(), "no WHERE would be a scan");
}

#[test]
fn a_refusal_keeps_naming_the_declaration_that_would_fix_it() {
    let p = plan(MIXED).unwrap();
    let Served::No { reason } = &p.queries[0].served else { panic!("expected a refusal") };
    // Not a new opinion — the compiler's own text, which teaches the
    // fix rather than only saying no.
    assert!(reason.contains("total"), "the refusal names the column: {reason}");

    // And it stays anchored to the SQL, so a plan can point at a line.
    assert!(p.queries[0].line > 0);
}

#[test]
fn a_served_query_names_the_paths_it_rides() {
    let p = plan(MIXED).unwrap();
    let Served::Yes { paths, .. } = &p.queries[1].served else { panic!("expected served") };
    assert_eq!(paths, &["orders.status"]);
}

#[test]
fn the_declarations_come_out_alongside_the_verdicts() {
    let p = plan(MIXED).unwrap();
    assert_eq!(p.declares.len(), 1);
    assert_eq!(p.declares[0][0], "TABLE.DECLARE");
    assert_eq!(p.declares[0][1], "orders");
}

#[test]
fn a_schema_that_does_not_parse_has_no_plan() {
    // The deliberate line: a broken view is an entry, a broken schema
    // is an error, because a plan against no schema is a fiction.
    let err = plan("CREATE TABLE orders (id bigserial PRIMARY KEY,;").unwrap_err();
    assert!(err.line > 0, "the error stays anchored: {err}");
}

#[test]
fn a_view_reading_an_undeclared_table_is_an_entry_not_an_error() {
    let sql = r"
CREATE TABLE orders (id bigserial PRIMARY KEY, status text);
CREATE INDEX ON orders (status);
CREATE VIEW ghosts AS SELECT * FROM shipments WHERE status = 'x';
CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';
";
    let p = plan(sql).unwrap();
    assert_eq!(p.unserved(), 1);
    assert!(p.queries[1].served.is_served(), "the later view is still planned");
}

/// The V2 drill's second wall: one refused type in one table must not
/// kill the whole plan. The billing-shaped table drops with a named
/// reason; the healthy tables still declare; a view over the dropped
/// table points at the drop instead of "unknown table".
#[test]
fn a_dropped_table_keeps_the_rest_of_the_plan() {
    let p = kevy_sql::plan(
        "CREATE TABLE users (id bigint PRIMARY KEY, email text);
         CREATE TABLE billing (id bigint PRIMARY KEY, amount money);
         CREATE INDEX ON billing (amount);
         CREATE VIEW v AS SELECT * FROM billing WHERE id = 1;",
    )
    .expect("the plan survives the dropped table");
    assert_eq!(p.declares.len(), 1, "users still declares");
    assert_eq!(p.dropped.len(), 1, "billing drops once: {:?}", p.dropped);
    assert!(p.dropped[0].1.contains("money"), "named reason: {}", p.dropped[0].1);
    let kevy_sql::Served::No { reason } = &p.queries[0].served else {
        panic!("the view over the dropped table cannot be served");
    };
    assert!(reason.contains("not declarable"), "points at the drop: {reason}");
}

/// The pg_dump dialect end to end (V2 drill, third wall): psql meta
/// lines, SET preamble, set_config, schema-qualified names, OWNER,
/// constraint-carried PRIMARY KEYs, USING btree, and PG's canonical
/// timestamp spelling — every one of these appears in every real
/// dump, and every one used to be fatal.
#[test]
fn a_real_pg_dump_compiles() {
    let dump = r"
\restrict abcDEF123
SET statement_timeout = 0;
SELECT pg_catalog.set_config('search_path', '', false);
CREATE TABLE public.users (
    id bigint NOT NULL,
    email text NOT NULL,
    created_at timestamp without time zone NOT NULL
);
ALTER TABLE public.users OWNER TO postgres;
ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);
CREATE UNIQUE INDEX users_email_idx ON public.users USING btree (email);
\unrestrict abcDEF123
";
    let p = kevy_sql::plan(dump).expect("the dump dialect compiles");
    assert_eq!(p.declares.len(), 1, "users declares: {:?}", p.dropped);
    assert!(p.dropped.is_empty(), "nothing drops: {:?}", p.dropped);
    let argv = p.declares[0].join(" ");
    assert!(argv.contains("users"), "declare: {argv}");
    assert!(argv.contains("id"), "the ALTER-carried PK folded back: {argv}");
    assert!(argv.contains("email"), "the unique index attached: {argv}");
}
