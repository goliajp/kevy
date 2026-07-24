//! Compiler semantics: schema → exact argv, the type-mapping table,
//! view planning (engine views / cards / named missing-declaration
//! errors), parameter slots, render_script shape, determinism.

use kevy_sql::{Compilation, KevyType, compile};

const SHOP: &str = r"
-- the cookbook schema
CREATE TABLE users (
  id     bigserial PRIMARY KEY,
  email  text,
  name   text,
  plan   text
);
CREATE UNIQUE INDEX ON users (email);

CREATE TABLE orders (
  id          bigserial PRIMARY KEY,
  user_id     bigint,
  status      text,
  total       numeric(10,2),
  created_at  bigint
);
CREATE INDEX ON orders (status) INCLUDE (total, created_at);
CREATE INDEX ON orders (user_id, created_at DESC);

CREATE VIEW paid_orders AS
  SELECT * FROM orders WHERE status = 'paid';

CREATE VIEW recent_orders_by_user AS
  SELECT id, status, total, created_at FROM orders
  WHERE user_id = $1
  ORDER BY created_at DESC
  LIMIT 20;
";

fn argv_strs(c: &Compilation, i: usize) -> Vec<&str> {
    c.commands[i].iter().map(String::as_str).collect()
}

#[test]
fn shop_schema_exact_argv() {
    let c = compile(SHOP).unwrap();
    assert_eq!(c.commands.len(), 3, "2 tables + 1 engine view");
    assert_eq!(
        argv_strs(&c, 0),
        [
            "TABLE.DECLARE", "users", "PREFIX", "users:", "PK", "id",
            "COLUMN", "id", "i64", "COLUMN", "email", "str",
            "COLUMN", "name", "str", "COLUMN", "plan", "str",
            "INDEX", "email", "unique",
        ]
    );
    assert_eq!(
        argv_strs(&c, 1),
        [
            "TABLE.DECLARE", "orders", "PREFIX", "orders:", "PK", "id",
            "COLUMN", "id", "i64", "COLUMN", "user_id", "i64",
            "COLUMN", "status", "str", "COLUMN", "total", "f64",
            "COLUMN", "created_at", "i64",
            "INDEX", "status", "range", "VALUES", "total", "created_at",
            "ORDERPATH", "user_id_created_at", "ON", "user_id", "THEN", "created_at", "DESC",
        ]
    );
    assert_eq!(
        argv_strs(&c, 2),
        [
            "VIEW.CREATE", "paid_orders", "QUERY", "orders.status", "EQ", "paid",
            "ORDER", "BY", "orders.status",
        ]
    );
    assert_eq!(c.query_cards.len(), 1);
    let card = &c.query_cards[0];
    assert_eq!(card.name, "recent_orders_by_user");
    assert_eq!(
        card.argv,
        [
            "IDX.QUERY", "orders.user_id_created_at", "WHERE", "user_id", "EQ", "$1",
            "LIMIT", "20", "FIELDS", "id", "status", "total", "created_at",
        ]
    );
    assert_eq!(card.params.len(), 1);
    assert_eq!(card.params[0].n, 1);
    assert_eq!(card.params[0].column, "user_id");
    assert_eq!(card.params[0].ty, KevyType::I64);
}

#[test]
fn type_mapping_table() {
    let c = compile(
        "CREATE TABLE m (
            a int PRIMARY KEY, b integer, c bigint, d serial, e bigserial,
            f real, g float, h double precision, i numeric, j decimal(10,2),
            k text, l varchar(255), m char(8), n uuid, o timestamp,
            p timestamptz, q date, r bool, s boolean, t json, u jsonb);",
    )
    .unwrap();
    let argv = &c.commands[0];
    let ty_of = |col: &str| {
        let i = argv
            .windows(2)
            .position(|w| w[0] == "COLUMN" && w[1] == col)
            .unwrap();
        argv[i + 2].clone()
    };
    for col in ["a", "b", "c", "d", "e"] {
        assert_eq!(ty_of(col), "i64", "{col}");
    }
    for col in ["f", "g", "h", "i", "j"] {
        assert_eq!(ty_of(col), "f64", "{col}");
    }
    for col in ["k", "l", "m", "n", "o", "p", "q", "r", "s", "t", "u"] {
        assert_eq!(ty_of(col), "str", "{col}");
    }
    // The coarse mapping is documented per column, honestly.
    assert!(c.notes.iter().any(|n| n.contains("m.d: serial") && n.contains("do NOT auto-increment")));
    assert!(c.notes.iter().any(|n| n.contains("m.o: timestamp") && n.contains("app-encoded time")));
    assert!(c.notes.iter().any(|n| n.contains("m.u: jsonb")));
}

#[test]
fn keywords_case_insensitive_and_quoting() {
    let c = compile(
        "create table \"Mixed\" (\"Id\" BIGINT primary key, x TEXT); \n\
         Create Index On \"Mixed\" (x); /* block */ -- tail",
    )
    .unwrap();
    assert_eq!(c.commands[0][1], "Mixed", "quoted identifiers keep case");
    assert_eq!(c.commands[0][5], "Id");
    // Unquoted identifiers fold to lowercase (PG folding).
    let c2 = compile("CREATE TABLE Users (ID bigint PRIMARY KEY);").unwrap();
    assert_eq!(c2.commands[0][1], "users");
    assert_eq!(c2.commands[0][5], "id");
}

#[test]
fn pk_rules() {
    // Table-level PK.
    let c = compile("CREATE TABLE t (a bigint, PRIMARY KEY (a));").unwrap();
    assert_eq!(c.commands[0][5], "a");
    // No PK is a named error.
    let e = compile("CREATE TABLE t (a bigint);").unwrap_err();
    assert!(e.message.contains("no PRIMARY KEY"), "{e}");
    // Two PKs.
    let e = compile("CREATE TABLE t (a bigint PRIMARY KEY, b bigint PRIMARY KEY);").unwrap_err();
    assert!(e.message.contains("more than one PRIMARY KEY"), "{e}");
    // UNIQUE (col) compiles to a unique index.
    let c = compile("CREATE TABLE t (a bigint PRIMARY KEY, b text, UNIQUE (b));").unwrap();
    let a: Vec<&str> = c.commands[0].iter().map(String::as_str).collect();
    assert!(a.windows(3).any(|w| w == ["INDEX", "b", "unique"]));
}

#[test]
fn schema_errors_are_named() {
    let e = compile("CREATE INDEX ON ghost (a);").unwrap_err();
    assert!(e.message.contains("unknown table 'ghost'"), "{e}");
    let e = compile("CREATE TABLE t (a bigint PRIMARY KEY); CREATE INDEX ON t (nope);").unwrap_err();
    assert!(e.message.contains("unknown column 'nope'"), "{e}");
    let e = compile("CREATE TABLE t (a bigint PRIMARY KEY); CREATE VIEW v AS SELECT * FROM ghost WHERE a = 1;")
        .unwrap_err();
    assert!(e.message.contains("FROM unknown table 'ghost'"), "{e}");
    let e = compile("CREATE TABLE t (a bigint PRIMARY KEY); CREATE TABLE t (a bigint PRIMARY KEY);")
        .unwrap_err();
    assert!(e.message.contains("duplicate table 't'"), "{e}");
    let e = compile("CREATE TABLE t (a bigint PRIMARY KEY, a text);").unwrap_err();
    assert!(e.message.contains("duplicate column 'a'"), "{e}");
    let e = compile("CREATE TABLE t (a bigint PRIMARY KEY); CREATE INDEX ON t (a); CREATE INDEX ON t (a);")
        .unwrap_err();
    assert!(e.message.contains("duplicate index on column 'a'"), "{e}");
}

const DEPT: &str = "
CREATE TABLE emp (
  id bigint PRIMARY KEY, dept text, age bigint, name text, salary real);
";

#[test]
fn missing_access_path_names_the_fix() {
    // The canonical error: WHERE (dept, age range) with nothing declared.
    let e = compile(&format!(
        "{DEPT}CREATE VIEW v AS SELECT * FROM emp WHERE dept = 'eng' AND age BETWEEN 30 AND 40;"
    ))
    .unwrap_err();
    assert!(e.message.contains("view 'v'"), "{e}");
    assert!(e.message.contains("WHERE (dept EQ, age range) matches no declared access path"), "{e}");
    assert!(e.message.contains("add: CREATE INDEX ON emp (dept, age)"), "{e}");

    // Single missing index.
    let e = compile(&format!("{DEPT}CREATE VIEW v AS SELECT * FROM emp WHERE dept = 'eng';"))
        .unwrap_err();
    assert!(e.message.contains("add: CREATE INDEX ON emp (dept)"), "{e}");

    // ORDER BY direction folds into the suggestion.
    let e = compile(&format!(
        "{DEPT}CREATE VIEW v AS SELECT * FROM emp WHERE dept = $1 ORDER BY age DESC;"
    ))
    .unwrap_err();
    assert!(e.message.contains("add: CREATE INDEX ON emp (dept, age DESC)"), "{e}");
}

#[test]
fn residual_needs_include_error() {
    let e = compile(&format!(
        "{DEPT}CREATE INDEX ON emp (dept);
         CREATE VIEW v AS SELECT * FROM emp WHERE dept = $1 AND age >= 30;"
    ))
    .unwrap_err();
    assert!(e.message.contains("residual WHERE on 'age'"), "{e}");
    assert!(e.message.contains("add INCLUDE (age) to CREATE INDEX ON emp (dept)"), "{e}");
}

#[test]
fn residual_filter_compiles_when_included() {
    let c = compile(&format!(
        "{DEPT}CREATE INDEX ON emp (dept) INCLUDE (age, name);
         CREATE VIEW v AS SELECT name FROM emp WHERE dept = $1 AND age >= 30 ORDER BY name ASC;"
    ))
    .unwrap();
    assert_eq!(
        c.query_cards[0].argv,
        [
            "IDX.QUERY", "emp.dept", "EQ", "$1",
            "FILTER", "age", "RANGE", "30", "9223372036854775807",
            "SORT", "name", "ASC", "FIELDS", "name",
        ]
    );
}

/// The view emitted for a constant single-pred query (constant
/// predicates compile to engine views; the bounds live in the leaf).
fn leaf_of(c: &Compilation) -> Vec<String> {
    c.commands.last().unwrap().clone()
}

#[test]
fn strict_bounds_adjust_integers_exactly() {
    let base = format!("{DEPT}CREATE INDEX ON emp (age);\n");
    let c = compile(&format!("{base}CREATE VIEW v AS SELECT * FROM emp WHERE age > 30 AND age < 40;")).unwrap();
    assert_eq!(
        leaf_of(&c),
        ["VIEW.CREATE", "v", "QUERY", "emp.age", "RANGE", "31", "39", "ORDER", "BY", "emp.age"]
    );
    // Strict on f64 refuses by name.
    let e = compile(&format!(
        "{DEPT}CREATE INDEX ON emp (salary);\nCREATE VIEW v AS SELECT * FROM emp WHERE salary > 9.5;"
    ))
    .unwrap_err();
    assert!(e.message.contains("strict '>' on a f64 bound"), "{e}");
    assert!(e.message.contains("use >= / <= / BETWEEN"), "{e}");
    // Overflow refuses.
    let e = compile(&format!(
        "{base}CREATE VIEW v AS SELECT * FROM emp WHERE age > 9223372036854775807;"
    ))
    .unwrap_err();
    assert!(e.message.contains("overflows i64"), "{e}");
}

#[test]
fn open_ranges_fill_type_extremes() {
    let base = format!("{DEPT}CREATE INDEX ON emp (age);\nCREATE INDEX ON emp (salary);\n");
    let c = compile(&format!("{base}CREATE VIEW v AS SELECT * FROM emp WHERE age >= 18;")).unwrap();
    assert_eq!(&leaf_of(&c)[3..7], ["emp.age", "RANGE", "18", "9223372036854775807"]);
    let c = compile(&format!("{base}CREATE VIEW v AS SELECT * FROM emp WHERE salary <= 9.5;")).unwrap();
    assert_eq!(&leaf_of(&c)[3..7], ["emp.salary", "RANGE", "-inf", "9.5"]);
    // str has no finite upper bound: named error.
    let e = compile(&format!(
        "{DEPT}CREATE INDEX ON emp (name);\nCREATE VIEW v AS SELECT * FROM emp WHERE name >= 'm';"
    ))
    .unwrap_err();
    assert!(e.message.contains("no finite upper bound"), "{e}");
    assert!(e.message.contains("BETWEEN"), "{e}");
}

#[test]
fn literal_typing_is_checked() {
    let base = format!("{DEPT}CREATE INDEX ON emp (age);\nCREATE INDEX ON emp (dept);\n");
    let e = compile(&format!("{base}CREATE VIEW v AS SELECT * FROM emp WHERE age = 'x';")).unwrap_err();
    assert!(e.message.contains("column 'age' is i64"), "{e}");
    let e = compile(&format!("{base}CREATE VIEW v AS SELECT * FROM emp WHERE dept = 42;")).unwrap_err();
    assert!(e.message.contains("quote the literal ('42')"), "{e}");
}

#[test]
fn constant_multi_leaf_engine_view() {
    let c = compile(&format!(
        "{DEPT}CREATE INDEX ON emp (dept);\nCREATE INDEX ON emp (age);
         CREATE VIEW eng30 AS SELECT * FROM emp WHERE dept = 'eng' AND age >= 30 ORDER BY age DESC;"
    ))
    .unwrap();
    assert_eq!(
        c.commands.last().unwrap().iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "VIEW.CREATE", "eng30", "QUERY",
            "(", "AND", "emp.dept", "EQ", "eng",
            "emp.age", "RANGE", "30", "9223372036854775807", ")",
            "ORDER", "BY", "emp.age", "DESC",
        ]
    );
    // The read template is in the notes.
    assert!(c.notes.iter().any(|n| n.contains("VIEW.QUERY eng30")), "{:?}", c.notes);
}

#[test]
fn offset_forces_a_card_even_when_constant() {
    let c = compile(&format!(
        "{DEPT}CREATE INDEX ON emp (dept);
         CREATE VIEW p2 AS SELECT * FROM emp WHERE dept = 'eng' LIMIT 10 OFFSET 10;"
    ))
    .unwrap();
    assert!(c.commands.iter().all(|a| a[0] != "VIEW.CREATE"));
    assert_eq!(
        c.query_cards[0].argv,
        [
            "IDX.QUERY", "emp.dept", "EQ", "eng", "OFFSET", "10", "LIMIT", "10",
            "FIELDS", "id", "dept", "age", "name", "salary",
        ]
    );
}

#[test]
fn where_is_required() {
    let e = compile(&format!("{DEPT}CREATE VIEW v AS SELECT * FROM emp;")).unwrap_err();
    assert!(e.message.contains("no WHERE"), "{e}");
    assert!(e.message.contains("IDX.QUERY"), "{e}");
}

#[test]
fn engine_caps_are_named() {
    let base = format!("{DEPT}CREATE INDEX ON emp (dept);\n");
    let e = compile(&format!("{base}CREATE VIEW v AS SELECT * FROM emp WHERE dept = $1 LIMIT 0;")).unwrap_err();
    assert!(e.message.contains("LIMIT 0"), "{e}");
    let e = compile(&format!("{base}CREATE VIEW v AS SELECT * FROM emp WHERE dept = $1 LIMIT 20000;")).unwrap_err();
    assert!(e.message.contains("caps LIMIT at 10000"), "{e}");
    let e = compile(&format!(
        "{base}CREATE VIEW v AS SELECT * FROM emp WHERE dept = $1 LIMIT 10 OFFSET 20000;"
    ))
    .unwrap_err();
    assert!(e.message.contains("caps OFFSET at 10000"), "{e}");
}

#[test]
fn select_and_order_validate_columns() {
    let base = format!("{DEPT}CREATE INDEX ON emp (dept);\n");
    let e = compile(&format!("{base}CREATE VIEW v AS SELECT ghost FROM emp WHERE dept = $1;")).unwrap_err();
    assert!(e.message.contains("SELECT names unknown column 'ghost'"), "{e}");
    let e = compile(&format!(
        "{base}CREATE VIEW v AS SELECT * FROM emp WHERE dept = $1 ORDER BY ghost;"
    ))
    .unwrap_err();
    assert!(e.message.contains("ORDER BY names unknown column 'ghost'"), "{e}");
    let e = compile(&format!("{base}CREATE VIEW v AS SELECT * FROM emp WHERE ghost = 1;")).unwrap_err();
    assert!(e.message.contains("WHERE names unknown column 'ghost'"), "{e}");
}

#[test]
fn param_type_conflict_is_refused() {
    let e = compile(&format!(
        "{DEPT}CREATE INDEX ON emp (dept) INCLUDE (age);
         CREATE VIEW v AS SELECT * FROM emp WHERE dept = $1 AND age = $1;"
    ))
    .unwrap_err();
    assert!(e.message.contains("$1 binds both"), "{e}");
}

#[test]
fn unnamed_single_index_note_and_named_composite() {
    let c = compile(&format!(
        "{DEPT}CREATE INDEX emp_dept ON emp (dept);\nCREATE INDEX by_age ON emp (dept, age);"
    ))
    .unwrap();
    assert!(c.notes.iter().any(|n| n.contains("index emp_dept") && n.contains("emp.dept")));
    let a: Vec<&str> = c.commands[0].iter().map(String::as_str).collect();
    assert!(a.windows(4).any(|w| w == ["ORDERPATH", "by_age", "ON", "dept"]));
}

#[test]
fn orderpath_eq_plus_range_card() {
    let c = compile(&format!(
        "{DEPT}CREATE INDEX ON emp (dept, age);
         CREATE VIEW v AS SELECT name FROM emp WHERE dept = $1 AND age BETWEEN 30 AND 40;"
    ))
    .unwrap();
    assert_eq!(
        c.query_cards[0].argv,
        [
            "IDX.QUERY", "emp.dept_age", "WHERE", "dept", "EQ", "$1",
            "RANGE", "age", "30", "40", "FIELDS", "name",
        ]
    );
}

#[test]
fn orderpath_direction_mismatch_names_the_fix() {
    // The declared path is (dept, age ASC); the view wants age DESC —
    // no match, and the error suggests the DESC declaration.
    let e = compile(&format!(
        "{DEPT}CREATE INDEX ON emp (dept, age);
         CREATE VIEW v AS SELECT * FROM emp WHERE dept = $1 ORDER BY age DESC;"
    ))
    .unwrap_err();
    assert!(e.message.contains("add: CREATE INDEX ON emp (dept, age DESC)"), "{e}");
}

#[test]
fn deterministic_output() {
    let a = compile(SHOP).unwrap();
    let b = compile(SHOP).unwrap();
    assert_eq!(a, b);
    assert_eq!(a.render_script(), b.render_script());
}

#[test]
fn render_script_shape() {
    let s = compile(SHOP).unwrap().render_script();
    assert!(s.contains("TABLE.DECLARE users PREFIX users: PK id"), "{s}");
    assert!(s.contains("VIEW.CREATE paid_orders QUERY orders.status EQ paid ORDER BY orders.status"), "{s}");
    assert!(s.contains("# ---- query card: recent_orders_by_user ----"), "{s}");
    assert!(s.contains("#   $1 = user_id (i64)"), "{s}");
    assert!(s.contains("#@card recent_orders_by_user"), "{s}");
    assert!(s.contains("#@param 1 user_id i64"), "{s}");
    assert!(
        s.contains("#@argv IDX.QUERY\torders.user_id_created_at\tWHERE\tuser_id\tEQ\t$1"),
        "{s}"
    );
    // Values with spaces are shell-quoted on command lines.
    let c = compile(
        "CREATE TABLE t (id bigint PRIMARY KEY, s text);
         CREATE INDEX ON t (s);
         CREATE VIEW v AS SELECT * FROM t WHERE s = 'two words';",
    )
    .unwrap();
    assert!(c.render_script().contains("EQ 'two words'"), "{}", c.render_script());
}
