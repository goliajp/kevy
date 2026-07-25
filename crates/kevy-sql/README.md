# kevy-sql

**Your schema's access paths, compiled — not a drop-in PG.**

kevy-sql is a *declaration-time* SQL compiler for [kevy]: it reads a
PG/MySQL-flavoured schema file ONCE — like a migration tool — and emits
explicit kevy engine commands plus ready-made query templates:

- `CREATE TABLE` → one `TABLE.DECLARE` (typed columns, PK, prefix);
- `CREATE [UNIQUE] INDEX` → declared Range / Unique access paths;
  multi-column indexes → composite `ORDERPATH`s; PG-style
  `INCLUDE (…)` covering columns → stored `VALUES` for residual
  FILTER/SORT;
- single-table `CREATE VIEW … AS SELECT` → an engine `VIEW.CREATE`
  (constant predicates) or a **query card**: the exact
  `IDX.QUERY …` argv with `$N` slots your app fills at runtime.

Nothing here ever runs per-query inside a serving process. Ad-hoc
runtime SQL stays refused by the engine itself (unknown command) —
that is kevy's Law 3: meaning and planning never enter the engine.

```text
$ kevy-cli sql compile schema.sql            # print the compiled script
$ kevy-cli sql compile schema.sql --apply --url 127.0.0.1:6004
```

Or as a library:

```rust
let c = kevy_sql::compile("CREATE TABLE t (id bigint PRIMARY KEY);")?;
for argv in &c.commands { /* send to the server */ }
println!("{}", c.render_script());
# Ok::<(), kevy_sql::SqlError>(())
```

## The compiler never plans

A view's WHERE either matches a *declared* access path (single-column
index, `INCLUDE`d stored columns, or a composite index by the
leading-prefix rule) — or the compile **errors naming the exact
declaration to add**:

```text
line 9, col 1: view 'v': WHERE (dept EQ, age range) matches no declared
access path — add: CREATE INDEX ON emp (dept, age)
```

Deterministic path choice (no cost model, no statistics):

1. constant predicates + expressible ORDER BY → an engine view;
2. all predicates on one indexed column → a direct-drive card;
3. the predicate set is a leading prefix of a composite index → a
   `WHERE` card on that ORDERPATH;
4. else the first indexed predicate drives and residuals compile to
   `FILTER` over the index's `INCLUDE` columns — or the error names
   what to add.

## What refuses, and how

Everything outside the subset errors **by name, with line/column**, and
the message teaches the kevy-shaped alternative instead of just saying
no:

```text
line 12, col 3: JOIN is not compilable — kevy refuses query-time joins
(Law 3); model the lookup with an indexed FK column (IDX.QUERY t.fk EQ …)
or app-side assembly (cookbook §2)
```

JOIN, subqueries, OR, GROUP BY/HAVING, expressions and function calls,
`!=`, LIKE, IS NULL, NOT NULL/DEFAULT/CHECK/FOREIGN KEY constraints,
INSERT/UPDATE/DELETE/ALTER — all named refusals.

## The coarse type mapping (honest)

kevy columns are `i64 | f64 | str` — nothing else:

| SQL | kevy | note |
|---|---|---|
| int, integer, bigint, serial, bigserial | `i64` | serial does **not** auto-increment — allocate ids app-side |
| real, float, double precision, numeric, decimal | `f64` | fixed-point becomes binary float; keep money as integer cents |
| text, varchar(n), char(n), uuid | `str` | length limits are not enforced |
| timestamp, timestamptz, date | `str` | app-encoded; use a sortable encoding (RFC3339 / zero-padded epoch) |
| bool, boolean | `str` | store `'0'`/`'1'` |
| json, jsonb | `str` | flatten indexed paths to their own columns |

The compiler emits a note per lossy column so the mapping is never
silent.

## Grammar (subset v1)

```ebnf
script      = { statement ";" } ;
statement   = create_table | create_index | create_view ;
create_table= "CREATE" "TABLE" name "(" item { "," item } ")" ;
item        = column type [ "PRIMARY" "KEY" ]
            | "PRIMARY" "KEY" "(" column ")"
            | "UNIQUE" "(" column ")" ;
create_index= "CREATE" [ "UNIQUE" ] "INDEX" [ name ] "ON" name
              "(" column [ "ASC" | "DESC" ] { "," column [ "ASC" | "DESC" ] } ")"
              [ "INCLUDE" "(" column { "," column } ")" ] ;
create_view = "CREATE" "VIEW" name "AS" "SELECT" ( "*" | column { "," column } )
              "FROM" name "WHERE" pred { "AND" pred }
              [ "ORDER" "BY" column [ "ASC" | "DESC" ] ]
              [ "LIMIT" int [ "OFFSET" int ] ] ;
pred        = column ( "=" | ">" | ">=" | "<" | "<=" ) value
            | column "BETWEEN" value "AND" value ;
value       = number | "'" string "'" | "$" int ;
```

Keywords are case-insensitive; unquoted identifiers fold to lowercase
(PG's rule); `"quoted"` identifiers keep their case; `--` and `/* */`
comments; `''` escapes inside strings. Strict `>` / `<` compile exactly
on integer literals (±1) and refuse elsewhere — `RANGE` bounds are
inclusive, and a silent off-by-epsilon would be an approximation, not a
compilation.

Part of the kevy workspace; pure Rust, zero crates.io dependencies
(the runtime crate is pure `std`).

[kevy]: https://crates.io/crates/kevy
