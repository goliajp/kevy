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
    View {
        /// The `table.column` paths this query rides, in argv order.
        paths: Vec<String>,
        /// The `VIEW.CREATE` argv.
        argv: Vec<String>,
    },
    /// Served by a runtime template the application binds and sends.
    Card {
        /// The `table.column` paths this query rides, in argv order.
        paths: Vec<String>,
        /// The query card.
        card: QueryCard,
    },
    /// Not served, with the compiler's own refusal — which names the
    /// alternative rather than only saying no.
    Refused {
        /// The refusal, verbatim.
        reason: String,
    },
}

impl Served {
    /// Whether this query can be served as declared.
    pub fn is_served(&self) -> bool {
        self.paths().is_some()
    }

    /// The `table.column` paths a served query rides, in argv order;
    /// `None` when it is refused.
    pub fn paths(&self) -> Option<&[String]> {
        match self {
            Served::View { paths, .. } | Served::Card { paths, .. } => Some(paths),
            Served::Refused { .. } => None,
        }
    }
}

/// One query's row in the plan.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PlanEntry {
    /// The view name.
    pub name: String,
    /// The table it reads.
    pub table: String,
    /// 1-based source line, so a refusal points back at the SQL.
    pub line: u32,
    /// The verdict.
    pub served: Served,
}

/// A migration plan: what to declare, and what becomes of each query.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Plan {
    /// `TABLE.DECLARE` argv per table, declaration order.
    pub declares: Vec<Vec<String>>,
    /// Every `CREATE VIEW`, served or not, declaration order.
    pub queries: Vec<PlanEntry>,
    /// Honest-mapping notes, as [`crate::Compilation::notes`].
    pub notes: Vec<String>,
    /// Tables (and stray indexes) that could NOT be declared:
    /// `(name, named reason)`. The charter's migration bar reads
    /// "every type either moved or named" — this is the named half.
    pub dropped: Vec<(String, String)>,
}

impl Plan {
    /// How many queries cannot be served as declared.
    pub fn unserved(&self) -> usize {
        self.queries.iter().filter(|q| !q.served.is_served()).count()
    }
}

/// Plan a whole SQL file: DDL becomes declarations, each `CREATE VIEW`
/// becomes an entry saying whether it can be served and by what.
///
/// Errors only on the schema itself — a file whose DDL does not parse
/// has no plan. A view that cannot be served comes back as an entry.
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
