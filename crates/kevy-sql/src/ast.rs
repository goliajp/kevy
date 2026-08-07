//! The parsed statement shapes — deliberately exactly as wide as the
//! compilable subset (everything wider errors by name in the parser).

use crate::KevyType;

/// One column of a `CREATE TABLE`.
#[derive(Debug, Clone)]
pub(crate) struct ColumnDef {
    pub(crate) name: String,
    /// `None` = the SQL type is outside the compilable subset; the
    /// build step turns that into the named error (or a dropped-table
    /// row on the lenient path).
    pub(crate) ty: Option<KevyType>,
    /// The SQL type as written (`bigserial`, `numeric`, …) — drives the
    /// honest-mapping notes.
    pub(crate) sql_ty: String,
    pub(crate) inline_pk: bool,
    /// `NOT NULL` was written — unenforceable here, carried as an
    /// honest-mapping note instead of a refusal (a real pg_dump has it
    /// on nearly every column; refusing it walls the first mile).
    pub(crate) not_null: bool,
    /// `DEFAULT <expr>` was written and dropped (note-carried).
    pub(crate) dropped_default: bool,
    pub(crate) line: u32,
    pub(crate) col: u32,
}

/// `CREATE TABLE <name> ( … );`
#[derive(Debug, Clone)]
pub(crate) struct CreateTable {
    pub(crate) name: String,
    pub(crate) columns: Vec<ColumnDef>,
    /// Table-level `PRIMARY KEY (col)`, with its anchor.
    pub(crate) pk: Option<(String, u32, u32)>,
    /// Table-level `UNIQUE (col)` constraints, declaration order.
    pub(crate) uniques: Vec<(String, u32, u32)>,
    pub(crate) line: u32,
    pub(crate) col: u32,
}

/// `CREATE [UNIQUE] INDEX [name] ON <t> (cols…) [INCLUDE (cols…)];`
#[derive(Debug, Clone)]
pub(crate) struct CreateIndex {
    pub(crate) unique: bool,
    pub(crate) name: Option<String>,
    pub(crate) table: String,
    /// `(column, desc)` in declaration order.
    pub(crate) cols: Vec<(String, bool)>,
    /// `INCLUDE (…)` covering columns → the index's stored `VALUES`.
    pub(crate) include: Vec<String>,
    pub(crate) line: u32,
    pub(crate) col: u32,
}

/// A predicate bound: a literal (kept as argv text) or a `$N` slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Bound {
    /// Numeric literal, original text.
    Num(String),
    /// String literal.
    Str(String),
    /// `$N` parameter.
    Param(u32),
}

/// The comparison of one WHERE predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PredOp {
    Eq,
    Gt,
    Ge,
    Lt,
    Le,
    Between,
}

/// One `col <op> value` / `col BETWEEN a AND b` predicate.
#[derive(Debug, Clone)]
pub(crate) struct Pred {
    pub(crate) column: String,
    pub(crate) op: PredOp,
    pub(crate) a: Bound,
    /// The second `BETWEEN` bound.
    pub(crate) b: Option<Bound>,
    pub(crate) line: u32,
    pub(crate) col: u32,
}

/// `CREATE VIEW <name> AS SELECT … FROM <t> [WHERE …] [ORDER BY …]
/// [LIMIT n [OFFSET m]];`
#[derive(Debug, Clone)]
pub(crate) struct CreateView {
    pub(crate) name: String,
    /// `None` = `SELECT *`.
    pub(crate) select: Option<Vec<String>>,
    pub(crate) table: String,
    /// AND-joined predicates, source order.
    pub(crate) preds: Vec<Pred>,
    /// `ORDER BY (column, desc)`.
    pub(crate) order: Option<(String, bool)>,
    pub(crate) limit: Option<u64>,
    pub(crate) offset: Option<u64>,
    pub(crate) line: u32,
    pub(crate) col: u32,
}

/// One statement of the compilable subset.
#[derive(Debug, Clone)]
pub(crate) enum Stmt {
    Table(CreateTable),
    Index(CreateIndex),
    View(CreateView),
}
