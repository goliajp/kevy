//! Every refusal path errors BY NAME, with line/column, and the message
//! teaches the kevy-shaped alternative — asserted verbatim enough that
//! the teaching can't silently rot.

use kevy_sql::{SqlError, compile};

fn err(sql: &str) -> SqlError {
    compile(sql).expect_err("must refuse")
}

/// (snippet, named token that must appear, line the error must anchor to)
#[track_caller]
fn refuses(sql: &str, name: &str, line: u32) {
    let e = err(sql);
    assert!(e.message.contains(name), "expected '{name}' in refusal, got: {e}");
    assert_eq!(e.line, line, "wrong line for {name}: {e}");
    assert!(e.col >= 1);
}

const T: &str = "CREATE TABLE t (id bigint PRIMARY KEY, a bigint, b text);\n";

#[test]
fn join_refused_teaches_via() {
    let e = err(&format!("{T}CREATE VIEW v AS SELECT * FROM t JOIN u ON t.a = u.a;"));
    assert_eq!(e.line, 2);
    assert!(e.message.contains("JOIN is not compilable"), "{e}");
    assert!(e.message.contains("Law 3"), "{e}");
    assert!(e.message.contains("cookbook \u{a7}2"), "{e}");
}

#[test]
fn statement_verbs_refused() {
    refuses("INSERT INTO t VALUES (1);", "INSERT is not compilable", 1);
    refuses("UPDATE t SET a = 1;", "UPDATE is not compilable", 1);
    refuses("DELETE FROM t;", "DELETE is not compilable", 1);
    refuses("ALTER TABLE t ADD COLUMN c text;", "ALTER is not compilable", 1);
    refuses("DROP TABLE t;", "DROP is not compilable", 1);
    refuses("SELECT * FROM t;", "SELECT is not compilable", 1);
    refuses("WITH x AS (SELECT 1) SELECT * FROM x;", "WITH is not compilable", 1);
    refuses("BEGIN;", "BEGIN is not compilable", 1);
    refuses("TRUNCATE t;", "TRUNCATE is not compilable", 1);
    refuses("GRANT ALL ON t TO u;", "GRANT is not compilable", 1);
}

#[test]
fn select_teaches_the_declared_path() {
    let e = err("SELECT * FROM t;");
    assert!(e.message.contains("declare it as CREATE VIEW"), "{e}");
}

#[test]
fn view_shapes_refused() {
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE a = 1 OR b = 'x';"),
        "OR is not compilable",
        2,
    );
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t GROUP BY a;"),
        "GROUP BY is not compilable",
        2,
    );
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t HAVING a = 1;"),
        "HAVING is not compilable",
        2,
    );
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE a IN (1, 2);"),
        "IN is not compilable",
        2,
    );
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE b LIKE 'x%';"),
        "LIKE is not compilable",
        2,
    );
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE b IS NULL;"),
        "IS is not compilable",
        2,
    );
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE a != 1;"),
        "'!=' / '<>' is not compilable",
        2,
    );
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE a <> 1;"),
        "'!=' / '<>' is not compilable",
        2,
    );
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE NOT a = 1;"),
        "NOT is not compilable",
        2,
    );
    refuses(&format!("{T}CREATE VIEW v AS SELECT * FROM t, u;"), "implicit join", 2);
    refuses(&format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE a = (SELECT 1);"), "subquery", 2);
    refuses(&format!("{T}CREATE VIEW v AS SELECT count(a) FROM t;"), "function call", 2);
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE a = 1 + 1;"),
        "arithmetic expression",
        2,
    );
    refuses(&format!("{T}CREATE VIEW v AS SELECT DISTINCT a FROM t;"), "SELECT DISTINCT", 2);
    refuses(&format!("{T}CREATE VIEW v AS SELECT a AS x FROM t;"), "column alias", 2);
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE a = 1 ORDER BY a, b;"),
        "multi-column ORDER BY",
        2,
    );
    refuses(&format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE a = b;"), "column reference", 2);
    refuses(
        &format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE a = 1 LIMIT $1;"),
        "parameterized LIMIT",
        2,
    );
    refuses(&format!("{T}CREATE VIEW v AS SELECT * FROM t UNION SELECT * FROM t;"), "UNION", 2);
}

#[test]
fn table_constraints_refused() {
    refuses(
        "CREATE TABLE t (id bigint PRIMARY KEY, a bigint REFERENCES u(id));",
        "REFERENCES is not compilable",
        1,
    );
    refuses(
        "CREATE TABLE t (id bigint PRIMARY KEY, a text UNIQUE);",
        "an inline UNIQUE is not compilable",
        1,
    );
    refuses(
        "CREATE TABLE t (id bigint PRIMARY KEY, CHECK (id > 0));",
        "CHECK is not compilable",
        1,
    );
    refuses(
        "CREATE TABLE t (id bigint PRIMARY KEY,\n  FOREIGN KEY (a) REFERENCES u(id));",
        "FOREIGN KEY is not compilable",
        2,
    );
    refuses("CREATE TABLE t (a bigint, b bigint, PRIMARY KEY (a, b));", "composite PRIMARY KEY", 1);
    refuses(
        "CREATE TABLE t (id bigint PRIMARY KEY, a text, b text, UNIQUE (a, b));",
        "multi-column UNIQUE constraint",
        1,
    );
}

#[test]
fn index_shapes_refused() {
    refuses(&format!("{T}CREATE UNIQUE INDEX ON t (a, b);"), "multi-column UNIQUE index", 2);
    refuses(&format!("{T}CREATE INDEX ON t (a, b) INCLUDE (b);"), "cannot carry INCLUDE", 2);
    refuses(&format!("{T}CREATE INDEX ON t (a DESC);"), "DESC on the single-column index", 2);
    refuses(&format!("{T}CREATE INDEX ON t USING gin (a);"), "USING <method>", 2);
    refuses(&format!("{T}CREATE INDEX ON t (a) WHERE a > 0;"), "partial index", 2);
    refuses(&format!("{T}CREATE INDEX ON t (a NULLS FIRST);"), "NULLS FIRST/LAST", 2);
}

#[test]
fn create_variants_refused() {
    refuses("CREATE OR REPLACE VIEW v AS SELECT 1;", "OR REPLACE", 1);
    refuses("CREATE MATERIALIZED VIEW v AS SELECT 1;", "CREATE MATERIALIZED VIEW", 1);
    refuses("CREATE TEMPORARY TABLE t (id bigint PRIMARY KEY);", "TEMPORARY", 1);
}

#[test]
fn type_refusals() {
    refuses(
        "CREATE TABLE t (id bigint PRIMARY KEY, a bytea);",
        "type 'bytea' is not in the compilable subset",
        1,
    );
    refuses("CREATE TABLE t (id bigint PRIMARY KEY, a text(5));", "takes no arguments", 1);
}

#[test]
fn lexer_refusals() {
    refuses("CREATE TABLE `t` (id bigint PRIMARY KEY);", "backtick", 1);
    refuses("CREATE TABLE t (id bigint PRIMARY KEY); /* open", "unterminated /* block comment", 1);
    refuses(
        "CREATE VIEW v AS SELECT * FROM t WHERE a = 'open;",
        "unterminated '\u{2026}' string literal",
        1,
    );
    refuses("CREATE TABLE t (id bigint PRIMARY KEY, a text); $x", "'$' must be followed", 1);
}

#[test]
fn bool_literal_teaches_encoding() {
    let e = err(&format!("{T}CREATE VIEW v AS SELECT * FROM t WHERE b = true;"));
    assert!(e.message.contains("bool literal"), "{e}");
    assert!(e.message.contains("'1' / '0'"), "{e}");
}

#[test]
fn line_and_col_anchor_the_offender() {
    // JOIN on line 12, as in the canonical example.
    let mut sql = String::from(T);
    for _ in 0..9 {
        sql.push('\n');
    }
    sql.push_str("CREATE VIEW v AS SELECT * FROM t\n  JOIN u ON t.a = u.a;");
    let e = err(&sql);
    assert_eq!(e.line, 12);
    assert_eq!(e.col, 3);
    assert_eq!(
        e.to_string(),
        format!("line 12, col 3: {}", e.message),
        "Display shape is the contract"
    );
}

/// NOT NULL / DEFAULT are NOT refusals: every real pg_dump carries
/// them on nearly every column, and a fatal error walls migration day
/// at the first mile (the V2 drill's own seed schema hit it). They
/// compile with an honest-mapping note each instead.
#[test]
fn unenforceable_column_constraints_become_notes() {
    let c = kevy_sql::compile(
        "CREATE TABLE t (id bigint PRIMARY KEY, a text NOT NULL, b bigint DEFAULT (1 + 2));",
    )
    .expect("NOT NULL / DEFAULT must compile");
    let notes = c.notes.join("\n");
    assert!(notes.contains("t.a: NOT NULL is not enforced"), "notes: {notes}");
    assert!(notes.contains("t.b: DEFAULT dropped"), "notes: {notes}");
}

/// PG's canonical long spellings compile — `timestamp with time zone`
/// is timestamptz (str, note-carried), not a refusal: pg_dump writes
/// the long form on every timestamptz column.
#[test]
fn timestamp_long_spellings_compile() {
    let c = compile("CREATE TABLE t (id bigint PRIMARY KEY, a timestamp with time zone);")
        .expect("timestamptz compiles");
    assert!(c.notes.join("\n").contains("timestamptz"), "{:?}", c.notes);
}
