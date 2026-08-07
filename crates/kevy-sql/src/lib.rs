//! kevy-sql — the OUT-OF-ENGINE, declaration-time SQL compiler.
//!
//! This tool compiles a schema ONCE, at declaration time, into explicit
//! kevy engine commands (`TABLE.DECLARE`, `VIEW.CREATE`) plus *query
//! cards* — ready-made `IDX.QUERY` templates an application binds
//! parameters into at runtime. It is a build step, like a schema
//! migration tool. **Nothing here ever runs per-query inside a serving
//! process**: ad-hoc runtime SQL stays refused by the engine itself
//! (unknown command), and this compiler refuses — by name, with
//! line/column — every SQL construct that would need query-time
//! evaluation (JOIN, subqueries, GROUP BY, expressions…). That is
//! kevy's Law 3: meaning and planning never enter the engine.
//!
//! The honest pitch: **your schema's access paths, compiled — not a
//! drop-in PG.** `CREATE TABLE` becomes a typed, verifiable
//! `TABLE.DECLARE`; `CREATE [UNIQUE] INDEX` becomes declared Range /
//! Unique / composite ORDERPATH access paths; single-table
//! `CREATE VIEW … AS SELECT` becomes either an engine view (constant
//! predicates) or a query card (parameterized / clause-bearing). The
//! compiler never plans: a WHERE clause either matches a declared
//! access path (leading-prefix rule) or the compile errors naming the
//! exact declaration to add.
//!
//! ```
//! let sql = "
//!     CREATE TABLE users (id bigint PRIMARY KEY, email text);
//!     CREATE UNIQUE INDEX ON users (email);
//! ";
//! let c = kevy_sql::compile(sql).unwrap();
//! assert_eq!(c.commands[0][0], "TABLE.DECLARE");
//! println!("{}", c.render_script());
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod ast;
mod lex;
mod parse;
mod parse_view;
mod plan;
mod render;
mod schema;
mod typemap;
mod viewplan;
mod viewplan_norm;
mod viewplan_view;

/// A compile error, anchored to the source: `line N, col C: message`.
///
/// Every refusal is *named* and teaches the kevy-shaped alternative —
/// e.g. `JOIN is not compilable — kevy refuses query-time joins
/// (Law 3); model the lookup with an indexed FK column …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    /// 1-based source line.
    pub line: u32,
    /// 1-based source column.
    pub col: u32,
    /// The named refusal / error text.
    pub message: String,
}

impl SqlError {
    pub(crate) fn at(line: u32, col: u32, message: impl Into<String>) -> SqlError {
        SqlError { line, col, message: message.into() }
    }
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}, col {}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for SqlError {}

/// A kevy column type — the deliberately coarse target of the SQL type
/// mapping (kevy columns are `i64 | f64 | str`; timestamps and the like
/// are app-encoded strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KevyType {
    /// 64-bit signed integer (`int`/`integer`/`bigint`/`serial`/`bigserial`).
    I64,
    /// 64-bit float (`real`/`float`/`double precision`/`numeric`/`decimal`).
    F64,
    /// Byte string (`text`/`varchar`/`char`/`uuid`/`timestamp`/`timestamptz`/
    /// `date`/`bool`/`boolean`/`json`/`jsonb`).
    Str,
}

impl KevyType {
    /// The wire tag (`i64` / `f64` / `str`) as it appears in
    /// `TABLE.DECLARE … COLUMN <name> <tag>`.
    pub fn tag(self) -> &'static str {
        match self {
            KevyType::I64 => "i64",
            KevyType::F64 => "f64",
            KevyType::Str => "str",
        }
    }
}

/// One `$N` parameter slot of a [`QueryCard`], with the column it binds
/// and that column's declared type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardParam {
    /// The 1-based parameter number (`$1` → 1).
    pub n: u32,
    /// The declared column the slot binds.
    pub column: String,
    /// The column's declared kevy type.
    pub ty: KevyType,
}

/// A compiled runtime template: the exact `IDX.QUERY …` argv with `$N`
/// slots left in place. The application substitutes real values for the
/// slots and sends the argv as-is — there is no runtime SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryCard {
    /// The view name the card compiles.
    pub name: String,
    /// The full argv template (`["IDX.QUERY", "orders.user_id", "EQ",
    /// "$1", …]`).
    pub argv: Vec<String>,
    /// The `$N` slots in ascending order.
    pub params: Vec<CardParam>,
}

/// The result of [`compile`]: engine commands, query cards, and notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compilation {
    /// Declaration commands in apply order: one `TABLE.DECLARE` per
    /// table (declaration order), then the `VIEW.CREATE`s. Each is an
    /// argv vector, ready for a RESP client.
    pub commands: Vec<Vec<String>>,
    /// Runtime templates for parameterized / clause-bearing views,
    /// declaration order.
    pub query_cards: Vec<QueryCard>,
    /// Honest-mapping notes (coarse type mappings, naming, read
    /// templates) — informational, never errors.
    pub notes: Vec<String>,
}

impl Compilation {
    /// Render the compilation as a readable text artifact: commands as
    /// pasteable lines, cards as documented comment blocks with a
    /// machine-readable `#@card` / `#@param` / `#@argv` section (argv
    /// tab-separated), then the notes. Deterministic.
    pub fn render_script(&self) -> String {
        render::render(self)
    }
}

pub use plan::{Plan, PlanEntry, Served, plan};

/// Compile a whole SQL schema file into a [`Compilation`].
///
/// Whole-file semantics: statements accumulate per table (a table's
/// `CREATE INDEX`es fold into its single `TABLE.DECLARE`), then each
/// `CREATE VIEW` is checked against the table's *declared* access
/// paths. The compiler never plans — a view whose WHERE has no
/// declared path errors naming the exact `CREATE INDEX` to add.
pub fn compile(sql: &str) -> Result<Compilation, SqlError> {
    let toks = lex::lex(sql)?;
    let stmts = parse::parse_script(&toks)?;
    let (tables, views, mut notes) = schema::build(&stmts)?;
    let mut commands: Vec<Vec<String>> = tables.iter().map(schema::declare_argv).collect();
    let mut query_cards = Vec::new();
    for v in &views {
        let Some(t) = tables.iter().find(|t| t.name == v.table) else {
            return Err(SqlError::at(
                v.line,
                v.col,
                format!(
                    "view '{}': FROM unknown table '{}' — CREATE TABLE it first (this compiler is whole-file: declare, then view)",
                    v.name, v.table
                ),
            ));
        };
        match viewplan::plan_view(v, t, &mut notes)? {
            viewplan::Planned::View(argv) => commands.push(argv),
            viewplan::Planned::Card(card) => query_cards.push(card),
        }
    }
    Ok(Compilation { commands, query_cards, notes })
}
