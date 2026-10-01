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
mod declaration_error;
mod declared;
mod fold;
mod fold_parse;
mod lex;
mod parse;
mod parse_dump;
mod parse_view;
mod plan;
mod render;
mod run;
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
///
/// ```
/// let e = kevy_sql::compile("CREATE TABLE t (id bigint PRIMARY KEY, x money);").unwrap_err();
/// assert_eq!((e.line, e.col), (1, 40));
/// assert!(e.to_string().starts_with("line 1, col 40: "));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SqlError {
    /// 1-based source line.
    ///
    /// ```
    /// let e = kevy_sql::compile("CREATE TABLE t (id bigint PRIMARY KEY, x money);").unwrap_err();
    /// assert_eq!(e.line, 1);
    /// ```
    pub line: u32,
    /// 1-based source column; here it points at the unmappable `money`.
    ///
    /// ```
    /// let e = kevy_sql::compile("CREATE TABLE t (id bigint PRIMARY KEY, x money);").unwrap_err();
    /// assert_eq!(e.col, 40);
    /// ```
    pub col: u32,
    /// The named refusal / error text.
    ///
    /// ```
    /// let e = kevy_sql::compile("CREATE TABLE t (id bigint PRIMARY KEY, x money);").unwrap_err();
    /// assert!(e.message.contains("money"));
    /// ```
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

/// One `$N` parameter slot of a [`QueryCard`], with the column it binds
/// and that column's declared type.
///
/// ```
/// let c = kevy_sql::compile(
///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
///      CREATE INDEX ON orders (user_id);
///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
/// )?;
/// let p = &c.query_cards[0].params[0];
/// assert_eq!((p.n, p.column.as_str(), p.ty), (1, "user_id", kevy_sql::ValType::I64));
/// # Ok::<(), kevy_sql::SqlError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CardParam {
    /// The 1-based parameter number (`$1` → 1).
    ///
    /// ```
    /// let c = kevy_sql::compile(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
    /// )?;
    /// assert_eq!(c.query_cards[0].params[0].n, 1);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub n: u32,
    /// The declared column the slot binds.
    ///
    /// ```
    /// let c = kevy_sql::compile(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
    /// )?;
    /// assert_eq!(c.query_cards[0].params[0].column, "user_id");
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub column: String,
    /// The column's declared kevy type (`i64`, `f64` or `str`: the SQL
    /// type mapping never yields another).
    ///
    /// ```
    /// let c = kevy_sql::compile(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
    /// )?;
    /// assert_eq!(c.query_cards[0].params[0].ty, kevy_sql::ValType::I64);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub ty: ValType,
}

/// A compiled runtime template: the exact `IDX.QUERY …` argv with `$N`
/// slots left in place. The application substitutes real values for the
/// slots and sends the argv as-is — there is no runtime SQL.
///
/// ```
/// let c = kevy_sql::compile(
///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
///      CREATE INDEX ON orders (user_id);
///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
/// )?;
/// let card = &c.query_cards[0];
/// // bind $1 = 42, then send the argv as-is
/// let argv: Vec<String> =
///     card.argv.iter().map(|a| if a == "$1" { "42".into() } else { a.clone() }).collect();
/// assert_eq!(argv[..4], ["IDX.QUERY", "orders.user_id", "EQ", "42"]);
/// # Ok::<(), kevy_sql::SqlError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct QueryCard {
    /// The view name the card compiles.
    ///
    /// ```
    /// let c = kevy_sql::compile(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
    /// )?;
    /// assert_eq!(c.query_cards[0].name, "by_user");
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub name: String,
    /// The full argv template (`["IDX.QUERY", "orders.user_id", "EQ",
    /// "$1", …]`).
    ///
    /// ```
    /// let c = kevy_sql::compile(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
    /// )?;
    /// assert_eq!(c.query_cards[0].argv[..4], ["IDX.QUERY", "orders.user_id", "EQ", "$1"]);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub argv: Vec<String>,
    /// The `$N` slots in ascending order.
    ///
    /// ```
    /// let c = kevy_sql::compile(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
    /// )?;
    /// let slots: Vec<u32> = c.query_cards[0].params.iter().map(|p| p.n).collect();
    /// assert_eq!(slots, [1]);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub params: Vec<CardParam>,
}

/// The result of [`compile`]: engine commands, query cards, and notes.
///
/// ```
/// let c = kevy_sql::compile(
///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
///      CREATE INDEX ON orders (user_id);
///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
/// )?;
/// assert_eq!(c.commands.len(), 2); // TABLE.DECLARE, then VIEW.CREATE
/// assert_eq!(c.query_cards.len(), 1);
/// # Ok::<(), kevy_sql::SqlError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Compilation {
    /// Declaration commands in apply order: one `TABLE.DECLARE` per
    /// table (declaration order), then the `VIEW.CREATE`s. Each is an
    /// argv vector, ready for a RESP client.
    ///
    /// ```
    /// let c = kevy_sql::compile(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
    /// )?;
    /// assert_eq!(c.commands[0][..2], ["TABLE.DECLARE", "orders"]);
    /// assert_eq!(c.commands[1][..2], ["VIEW.CREATE", "mine"]);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub commands: Vec<Vec<String>>,
    /// Runtime templates for parameterized / clause-bearing views,
    /// declaration order.
    ///
    /// ```
    /// let c = kevy_sql::compile(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
    /// )?;
    /// assert_eq!(c.query_cards[0].name, "by_user");
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub query_cards: Vec<QueryCard>,
    /// Honest-mapping notes (coarse type mappings, naming, read
    /// templates) — informational, never errors.
    ///
    /// ```
    /// let c = kevy_sql::compile("CREATE TABLE ev (id bigint PRIMARY KEY, at timestamp);")?;
    /// assert!(c.notes.iter().any(|n| n.contains("ev.at: timestamp → str")));
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub notes: Vec<String>,
}

impl Compilation {
    /// Render the compilation as a readable text artifact: commands as
    /// pasteable lines, cards as documented comment blocks with a
    /// machine-readable `#@card` / `#@param` / `#@argv` section (argv
    /// tab-separated), then the notes. Deterministic.
    ///
    /// ```
    /// let c = kevy_sql::compile(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;",
    /// )?;
    /// let script = c.render_script();
    /// assert!(script.contains("#@card by_user"));
    /// assert!(script.contains("#@param 1 user_id i64"));
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub fn render_script(&self) -> String {
        render::render(self)
    }
}

pub use declaration_error::DeclarationError;
pub use fold::{Folded, fold_select};
pub use kevy_index::ValType;
pub use kevy_scalar::Scalar;
pub use plan::{Plan, PlanEntry, Served, plan};
pub use run::{select_card, table_ddl};

/// Compile a whole SQL schema file into a [`Compilation`].
///
/// Whole-file semantics: statements accumulate per table (a table's
/// `CREATE INDEX`es fold into its single `TABLE.DECLARE`), then each
/// `CREATE VIEW` is checked against the table's *declared* access
/// paths. The compiler never plans — a view whose WHERE has no
/// declared path errors naming the exact `CREATE INDEX` to add.
///
/// ```
/// let c = kevy_sql::compile(
///     "CREATE TABLE orders (id bigint PRIMARY KEY, status text);
///      CREATE INDEX ON orders (status);
///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
/// )?;
/// assert_eq!(c.commands[1][..2], ["VIEW.CREATE", "paid"]);
///
/// // without the index the view has no access path, and the error says what to add
/// let e = kevy_sql::compile(
///     "CREATE TABLE orders (id bigint PRIMARY KEY, status text);
///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
/// )
/// .unwrap_err();
/// assert!(e.message.contains("add: CREATE INDEX ON orders (status)"));
/// # Ok::<(), kevy_sql::SqlError>(())
/// ```
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

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<SqlError>();
    send_sync::<DeclarationError>();
    send_sync::<CardParam>();
    send_sync::<QueryCard>();
    send_sync::<Compilation>();
    send_sync::<Folded>();
    send_sync::<Plan>();
    send_sync::<PlanEntry>();
    send_sync::<Served>();
};
