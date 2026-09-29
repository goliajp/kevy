//! `plan` — compile a schema and report **every** query's fate, instead
//! of stopping at the first one that cannot be served.
//!
//! [`compile`](crate::compile) is the build-time entry: one unservable
//! view is a compile error, which is right when the output is commands
//! you are about to apply. A migration plan is the other shape. The
//! person reading it arrived with a schema and forty queries and wants
//! one answer — *which of these work here, and what do the rest need?*
//! Stopping at the first refusal answers that one fortieth of the way.
//!
//! The line between the two failure kinds is deliberate:
//!
//! * **A DDL error is still fatal.** If `CREATE TABLE` does not parse
//!   there is no schema, and a plan against no schema is a fiction.
//! * **A view that cannot be served is an entry, not an error** — that
//!   is precisely what the plan exists to report. The refusal text
//!   already names the `CREATE INDEX` that would fix it, so it is
//!   carried verbatim rather than restated worse.

use crate::{QueryCard, SqlError, lex, parse, schema, viewplan};

/// Whether a declared query can be served, and by what.
///
/// ```
/// let p = kevy_sql::plan(
///     "CREATE TABLE t (id bigint PRIMARY KEY, n bigint);
///      CREATE INDEX ON t (n);
///      CREATE VIEW big AS SELECT * FROM t WHERE n >= 10;
///      CREATE VIEW odd AS SELECT * FROM t WHERE id = 1;",
/// )?;
/// assert!(matches!(p.queries[0].served, kevy_sql::Served::View { .. }));
/// assert_eq!(p.queries[0].served.paths(), Some(&["t.n".to_string()][..]));
/// assert!(!p.queries[1].served.is_served());
/// # Ok::<(), kevy_sql::SqlError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Served {
    /// Served by the engine holding the whole query as a view.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// let kevy_sql::Served::View { argv, .. } = &p.queries[0].served else { unreachable!() };
    /// assert_eq!(argv[..2], ["VIEW.CREATE", "mine"]);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    View {
        /// The `table.column` paths this query rides, in argv order.
        ///
        /// ```
        /// let p = kevy_sql::plan(
        ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
        ///      CREATE INDEX ON orders (user_id);
        ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
        ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
        ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
        /// )?;
        /// let kevy_sql::Served::View { paths, .. } = &p.queries[0].served else { unreachable!() };
        /// assert_eq!(paths, &["orders.user_id"]);
        /// # Ok::<(), kevy_sql::SqlError>(())
        /// ```
        paths: Vec<String>,
        /// The `VIEW.CREATE` argv.
        ///
        /// ```
        /// let p = kevy_sql::plan(
        ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
        ///      CREATE INDEX ON orders (user_id);
        ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
        ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
        ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
        /// )?;
        /// let kevy_sql::Served::View { argv, .. } = &p.queries[0].served else { unreachable!() };
        /// assert_eq!(argv.join(" "), "VIEW.CREATE mine QUERY orders.user_id EQ 7 ORDER BY orders.user_id");
        /// # Ok::<(), kevy_sql::SqlError>(())
        /// ```
        argv: Vec<String>,
    },
    /// Served by a runtime template the application binds and sends.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// let kevy_sql::Served::Card { card, .. } = &p.queries[1].served else { unreachable!() };
    /// assert_eq!(card.argv[..4], ["IDX.QUERY", "orders.user_id", "EQ", "$1"]);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    Card {
        /// The `table.column` paths this query rides, in argv order.
        ///
        /// ```
        /// let p = kevy_sql::plan(
        ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
        ///      CREATE INDEX ON orders (user_id);
        ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
        ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
        ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
        /// )?;
        /// let kevy_sql::Served::Card { paths, .. } = &p.queries[1].served else { unreachable!() };
        /// assert_eq!(paths, &["orders.user_id"]);
        /// # Ok::<(), kevy_sql::SqlError>(())
        /// ```
        paths: Vec<String>,
        /// The query card.
        ///
        /// ```
        /// let p = kevy_sql::plan(
        ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
        ///      CREATE INDEX ON orders (user_id);
        ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
        ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
        ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
        /// )?;
        /// let kevy_sql::Served::Card { card, .. } = &p.queries[1].served else { unreachable!() };
        /// assert_eq!((card.name.as_str(), card.params[0].column.as_str()), ("by_user", "user_id"));
        /// # Ok::<(), kevy_sql::SqlError>(())
        /// ```
        card: QueryCard,
    },
    /// Not served, with the compiler's own refusal — which names the
    /// alternative rather than only saying no.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// assert!(matches!(p.queries[2].served, kevy_sql::Served::Refused { .. }));
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    Refused {
        /// The refusal, verbatim.
        ///
        /// ```
        /// let p = kevy_sql::plan(
        ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
        ///      CREATE INDEX ON orders (user_id);
        ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
        ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
        ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
        /// )?;
        /// let kevy_sql::Served::Refused { reason } = &p.queries[2].served else { unreachable!() };
        /// assert!(reason.ends_with("add: CREATE INDEX ON orders (status)"));
        /// # Ok::<(), kevy_sql::SqlError>(())
        /// ```
        reason: String,
    },
}

impl Served {
    /// Whether this query can be served as declared.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// let served: Vec<bool> = p.queries.iter().map(|q| q.served.is_served()).collect();
    /// assert_eq!(served, [true, true, false]);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub fn is_served(&self) -> bool {
        self.paths().is_some()
    }

    /// The `table.column` paths a served query rides, in argv order;
    /// `None` when it is refused.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// assert_eq!(p.queries[0].served.paths(), Some(&["orders.user_id".to_string()][..]));
    /// assert_eq!(p.queries[2].served.paths(), None);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub fn paths(&self) -> Option<&[String]> {
        match self {
            Served::View { paths, .. } | Served::Card { paths, .. } => Some(paths),
            Served::Refused { .. } => None,
        }
    }
}

/// One query's row in the plan.
///
/// ```
/// let p = kevy_sql::plan(
///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
///      CREATE INDEX ON orders (user_id);
///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
/// )?;
/// let q = &p.queries[2];
/// assert_eq!((q.name.as_str(), q.table.as_str(), q.line), ("paid", "orders", 5));
/// assert!(!q.served.is_served());
/// # Ok::<(), kevy_sql::SqlError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PlanEntry {
    /// The view name.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// let names: Vec<&str> = p.queries.iter().map(|q| q.name.as_str()).collect();
    /// assert_eq!(names, ["mine", "by_user", "paid"]);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub name: String,
    /// The table it reads.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// assert!(p.queries.iter().all(|q| q.table == "orders"));
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub table: String,
    /// 1-based source line, so a refusal points back at the SQL.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// // the refused view is the fifth line of the SQL above
    /// assert_eq!(p.queries[2].line, 5);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub line: u32,
    /// The verdict.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// assert!(p.queries[0].served.is_served());
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub served: Served,
}

/// A migration plan: what to declare, and what becomes of each query.
///
/// ```
/// let p = kevy_sql::plan(
///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
///      CREATE INDEX ON orders (user_id);
///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
/// )?;
/// assert_eq!(p.declares.len(), 1);
/// assert_eq!(p.queries.len(), 3);
/// assert_eq!(p.unserved(), 1);
/// # Ok::<(), kevy_sql::SqlError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Plan {
    /// `TABLE.DECLARE` argv per table, declaration order.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// assert_eq!(p.declares[0][..2], ["TABLE.DECLARE", "orders"]);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub declares: Vec<Vec<String>>,
    /// Every `CREATE VIEW`, served or not, declaration order.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// assert_eq!(p.queries.len(), 3);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub queries: Vec<PlanEntry>,
    /// Honest-mapping notes, as [`crate::Compilation::notes`].
    ///
    /// ```
    /// let p = kevy_sql::plan("CREATE TABLE ev (id bigint PRIMARY KEY, at timestamp);")?;
    /// assert!(p.notes.iter().any(|n| n.contains("ev.at: timestamp → str")));
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub notes: Vec<String>,
    /// Tables (and stray indexes) that could NOT be declared:
    /// `(name, named reason)`. The charter's migration bar reads
    /// "every type either moved or named" — this is the named half.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE users (id bigint PRIMARY KEY, email text);
    ///      CREATE TABLE billing (id bigint PRIMARY KEY, amount money);",
    /// )?;
    /// assert_eq!(p.declares.len(), 1); // users still declares
    /// assert_eq!(p.dropped[0].0, "billing");
    /// assert!(p.dropped[0].1.contains("money"));
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub dropped: Vec<(String, String)>,
}

impl Plan {
    /// How many queries cannot be served as declared.
    ///
    /// ```
    /// let p = kevy_sql::plan(
    ///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
    ///      CREATE INDEX ON orders (user_id);
    ///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
    ///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
    ///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
    /// )?;
    /// assert_eq!(p.unserved(), 1);
    /// # Ok::<(), kevy_sql::SqlError>(())
    /// ```
    pub fn unserved(&self) -> usize {
        self.queries.iter().filter(|q| !q.served.is_served()).count()
    }
}

/// Plan a whole SQL file: DDL becomes declarations, each `CREATE VIEW`
/// becomes an entry saying whether it can be served and by what.
///
/// Errors only on the schema itself — a file whose DDL does not parse
/// has no plan. A view that cannot be served comes back as an entry.
///
/// ```
/// let p = kevy_sql::plan(
///     "CREATE TABLE orders (id bigint PRIMARY KEY, user_id bigint, status text);
///      CREATE INDEX ON orders (user_id);
///      CREATE VIEW mine AS SELECT * FROM orders WHERE user_id = 7;
///      CREATE VIEW by_user AS SELECT * FROM orders WHERE user_id = $1;
///      CREATE VIEW paid AS SELECT * FROM orders WHERE status = 'paid';",
/// )?;
/// // one refused view does not stop the other two from being planned
/// assert_eq!(p.queries.len(), 3);
/// assert_eq!(p.unserved(), 1);
///
/// // a schema that does not parse has no plan at all
/// assert!(kevy_sql::plan("CREATE TABLE (").is_err());
/// # Ok::<(), kevy_sql::SqlError>(())
/// ```
pub fn plan(sql: &str) -> Result<Plan, SqlError> {
    let toks = lex::lex(sql)?;
    let stmts = parse::parse_script(&toks)?;
    let ((tables, views, mut notes), dropped) = schema::build_lenient(&stmts);
    let declares: Vec<Vec<String>> = tables.iter().map(schema::declare_argv).collect();
    let mut queries = Vec::with_capacity(views.len());
    for v in &views {
        let served = match tables.iter().find(|t| t.name == v.table) {
            None => Served::Refused {
                reason: match dropped.iter().find(|(n, _)| *n == v.table) {
                    Some((_, why)) => {
                        format!("its table '{}' was not declarable — {why}", v.table)
                    }
                    None => format!(
                        "FROM unknown table '{}' — CREATE TABLE it first (this compiler is whole-file: declare, then view)",
                        v.table
                    ),
                },
            },
            Some(t) => match viewplan::plan_view(v, t, &mut notes) {
                Ok(viewplan::Planned::View(argv)) => {
                    Served::View { paths: paths_in(&argv, &t.name), argv }
                }
                Ok(viewplan::Planned::Card(card)) => {
                    Served::Card { paths: paths_in(&card.argv, &t.name), card }
                }
                Err(e) => Served::Refused { reason: e.message },
            },
        };
        queries.push(PlanEntry {
            name: v.name.clone(),
            table: v.table.clone(),
            line: v.line,
            served,
        });
    }
    Ok(Plan { declares, queries, notes, dropped })
}

/// The declared paths an argv rides, read off the argv rather than
/// re-derived: both `VIEW.CREATE` and `IDX.QUERY` name their paths as
/// `table.column`, so there is nothing here to infer.
fn paths_in(argv: &[String], table: &str) -> Vec<String> {
    let prefix = format!("{table}.");
    let mut out: Vec<String> = Vec::new();
    for a in argv {
        if a.starts_with(&prefix) && !out.contains(a) {
            out.push(a.clone());
        }
    }
    out
}
