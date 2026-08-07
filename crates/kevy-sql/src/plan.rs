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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// Served by declared access paths, named.
    Yes {
        /// The `table.column` paths this query rides, in argv order.
        paths: Vec<String>,
        /// The `VIEW.CREATE` argv, when the engine can hold the whole
        /// query as a view.
        view: Option<Vec<String>>,
        /// The runtime template, when it is a card instead.
        card: Option<QueryCard>,
    },
    /// Not served, with the compiler's own refusal — which names the
    /// alternative rather than only saying no.
    No {
        /// The refusal, verbatim.
        reason: String,
    },
}

impl Served {
    /// Whether this query can be served as declared.
    pub fn is_served(&self) -> bool {
        matches!(self, Served::Yes { .. })
    }
}

/// One query's row in the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// `TABLE.DECLARE` argv per table, declaration order.
    pub declares: Vec<Vec<String>>,
    /// Every `CREATE VIEW`, served or not, declaration order.
    pub queries: Vec<PlanEntry>,
    /// Honest-mapping notes, as [`crate::Compilation::notes`].
    pub notes: Vec<String>,
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
    let (tables, views, mut notes) = schema::build(&stmts)?;
    let declares: Vec<Vec<String>> = tables.iter().map(schema::declare_argv).collect();
    let mut queries = Vec::with_capacity(views.len());
    for v in &views {
        let served = match tables.iter().find(|t| t.name == v.table) {
            None => Served::No {
                reason: format!(
                    "FROM unknown table '{}' — CREATE TABLE it first (this compiler is whole-file: declare, then view)",
                    v.table
                ),
            },
            Some(t) => match viewplan::plan_view(v, t, &mut notes) {
                Ok(viewplan::Planned::View(argv)) => {
                    Served::Yes { paths: paths_in(&argv, &t.name), view: Some(argv), card: None }
                }
                Ok(viewplan::Planned::Card(card)) => Served::Yes {
                    paths: paths_in(&card.argv, &t.name),
                    view: None,
                    card: Some(card),
                },
                Err(e) => Served::No { reason: e.message },
            },
        };
        queries.push(PlanEntry {
            name: v.name.clone(),
            table: v.table.clone(),
            line: v.line,
            served,
        });
    }
    Ok(Plan { declares, queries, notes })
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
