//! Pass 1 — accumulate statements per table and emit ONE
//! `TABLE.DECLARE` argv per table, folding its indexes and orderpaths
//! in. All checks are compiler-side so every error carries the SQL
//! source line; the emitted argv additionally round-trips through
//! kevy-index's own wire parser in the test suite, so the two
//! implementations cannot drift.

use crate::ast::{CreateIndex, CreateView, Stmt};
use crate::{KevyType, SqlError, typemap};

/// Mirrors kevy-index `MAX_COMPOSITE_COLS` (asserted by the round-trip
/// test — a drift fails there, loudly).
const MAX_COMPOSITE_COLS: usize = 8;
/// Mirrors kevy-index `MAX_TABLES`.
const MAX_TABLES: usize = 64;

/// One declared secondary index (single column).
#[derive(Debug)]
pub(crate) struct Ix {
    pub(crate) column: String,
    pub(crate) unique: bool,
    /// `INCLUDE` covering columns → stored `VALUES`.
    pub(crate) values: Vec<String>,
}

/// One composite path (multi-column index).
#[derive(Debug)]
pub(crate) struct OrderPath {
    pub(crate) name: String,
    pub(crate) on: Vec<(String, bool)>,
}

/// One fully-accumulated table.
#[derive(Debug)]
pub(crate) struct Table {
    pub(crate) name: String,
    pub(crate) pk: String,
    /// `(name, type)` in declaration order.
    pub(crate) columns: Vec<(String, KevyType)>,
    pub(crate) indexes: Vec<Ix>,
    pub(crate) orderpaths: Vec<OrderPath>,
}

impl Table {
    pub(crate) fn column_type(&self, c: &str) -> Option<KevyType> {
        self.columns.iter().find(|(n, _)| n == c).map(|(_, t)| *t)
    }

    /// The key-prefix domain: `<table>:`.
    pub(crate) fn prefix(&self) -> String {
        format!("{}:", self.name)
    }

    pub(crate) fn index_on(&self, col: &str) -> Option<&Ix> {
        self.indexes.iter().find(|ix| ix.column == col)
    }
}

type Built = (Vec<Table>, Vec<CreateView>, Vec<String>);

/// Accumulate all statements: tables (with their indexes folded in),
/// views (compiled in pass 2), honest-mapping notes.
pub(crate) fn build(stmts: &[Stmt]) -> Result<Built, SqlError> {
    let mut tables: Vec<Table> = Vec::new();
    let mut views: Vec<CreateView> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for s in stmts {
        match s {
            Stmt::Table(t) => {
                let built = build_table(t, &tables, &mut notes)?;
                if tables.len() >= MAX_TABLES {
                    return Err(SqlError::at(t.line, t.col, "table limit reached (64)"));
                }
                tables.push(built);
            }
            Stmt::Index(ix) => attach_index(ix, &mut tables, &mut notes)?,
            Stmt::View(v) => views.push(v.clone()),
        }
    }
    Ok((tables, views, notes))
}

/// [`build`], but a table that cannot be declared becomes a
/// `(name, reason)` row instead of killing the whole build — the plan
/// face's contract ("report every fate") extended down to DDL: a
/// pg_dump with one refused type in one table must still yield a plan
/// for the other twenty (the V2 drill's second wall). Indexes and
/// views over a dropped table are skipped; the table's own row
/// already names why.
pub(crate) fn build_lenient(stmts: &[Stmt]) -> (Built, Vec<(String, String)>) {
    let mut tables: Vec<Table> = Vec::new();
    let mut views: Vec<CreateView> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut dropped: Vec<(String, String)> = Vec::new();
    for s in stmts {
        match s {
            Stmt::Table(t) => match build_table(t, &tables, &mut notes) {
                Ok(built) if tables.len() < MAX_TABLES => tables.push(built),
                Ok(_) => dropped.push((t.name.clone(), "table limit reached (64)".into())),
                Err(e) => dropped.push((t.name.clone(), e.message)),
            },
            Stmt::Index(ix) => {
                if dropped.iter().any(|(n, _)| *n == ix.table) {
                    continue;
                }
                if let Err(e) = attach_index(ix, &mut tables, &mut notes) {
                    dropped.push((format!("index on {}", ix.table), e.message));
                }
            }
            Stmt::View(v) => views.push(v.clone()),
        }
    }
    ((tables, views, notes), dropped)
}

/// The honest-mapping notes one column carries: the unenforceable
/// constraints it wrote (NOT NULL / DEFAULT — note-carried, never
/// refused, or every real pg_dump would wall at the first mile) plus
/// the type-mapping note when the mapping loses something.
fn column_notes(table: &str, c: &crate::ast::ColumnDef, notes: &mut Vec<String>) {
    if c.not_null {
        notes.push(format!(
            "{table}.{}: NOT NULL is not enforced — kevy enforces no schema at \
             write time (Law 3); an absent field reads as NULL and the row \
             simply leaves the index",
            c.name
        ));
    }
    if c.dropped_default {
        notes.push(format!(
            "{table}.{}: DEFAULT dropped — kevy enforces no schema; write the \
             default value app-side",
            c.name
        ));
    }
    if let Some(n) = typemap::mapping_note(table, &c.name, &c.sql_ty) {
        notes.push(n);
    }
}

fn build_table(
    t: &crate::ast::CreateTable,
    existing: &[Table],
    notes: &mut Vec<String>,
) -> Result<Table, SqlError> {
    if existing.iter().any(|e| e.name == t.name) {
        return Err(SqlError::at(t.line, t.col, format!("duplicate table '{}'", t.name)));
    }
    let mut columns = Vec::new();
    for c in &t.columns {
        if columns.iter().any(|(n, _)| n == &c.name) {
            return Err(SqlError::at(c.line, c.col, format!("duplicate column '{}'", c.name)));
        }
        column_notes(&t.name, c, notes);
        let Some(ty) = c.ty else {
            return Err(SqlError::at(
                c.line,
                c.col,
                format!(
                    "type '{}' is not in the compilable subset \u{2014} kevy \
                     columns are i64|f64|str; map it explicitly (type table: \
                     cookbook \u{a7}22)",
                    c.sql_ty
                ),
            ));
        };
        columns.push((c.name.clone(), ty));
    }
    let pk = resolve_pk(t)?;
    let mut table =
        Table { name: t.name.clone(), pk, columns, indexes: Vec::new(), orderpaths: Vec::new() };
    for (u, line, col) in &t.uniques {
        if table.column_type(u).is_none() {
            return Err(SqlError::at(*line, *col, format!("UNIQUE names unknown column '{u}'")));
        }
        push_index(&mut table, Ix { column: u.clone(), unique: true, values: Vec::new() }, *line, *col)?;
    }
    Ok(table)
}

/// Exactly one primary key, inline or table-level.
fn resolve_pk(t: &crate::ast::CreateTable) -> Result<String, SqlError> {
    let inline: Vec<&crate::ast::ColumnDef> =
        t.columns.iter().filter(|c| c.inline_pk).collect();
    match (&t.pk, inline.as_slice()) {
        (Some((pk, line, col)), []) => {
            if !t.columns.iter().any(|c| &c.name == pk) {
                return Err(SqlError::at(
                    *line,
                    *col,
                    format!("PRIMARY KEY names unknown column '{pk}'"),
                ));
            }
            Ok(pk.clone())
        }
        (None, [c]) => Ok(c.name.clone()),
        (None, []) => Err(SqlError::at(
            t.line,
            t.col,
            format!(
                "table '{}' has no PRIMARY KEY \u{2014} kevy rows live at <prefix><pk>; add one",
                t.name
            ),
        )),
        _ => Err(SqlError::at(t.line, t.col, "more than one PRIMARY KEY".to_string())),
    }
}

fn push_index(t: &mut Table, ix: Ix, line: u32, col: u32) -> Result<(), SqlError> {
    if t.indexes.iter().any(|e| e.column == ix.column) {
        return Err(SqlError::at(
            line,
            col,
            format!("duplicate index on column '{}' of table '{}'", ix.column, t.name),
        ));
    }
    t.indexes.push(ix);
    Ok(())
}

/// Attach one `CREATE [UNIQUE] INDEX` to its table: single column →
/// Range/Unique index (with `INCLUDE` → `VALUES`); multi-column →
/// composite ORDERPATH (auto-named `<a>_<b>` when unnamed).
fn attach_index(
    ix: &CreateIndex,
    tables: &mut [Table],
    notes: &mut Vec<String>,
) -> Result<(), SqlError> {
    let Some(t) = tables.iter_mut().find(|t| t.name == ix.table) else {
        return Err(SqlError::at(
            ix.line,
            ix.col,
            format!("INDEX ON unknown table '{}' \u{2014} CREATE TABLE it first", ix.table),
        ));
    };
    for (c, _) in &ix.cols {
        if t.column_type(c).is_none() {
            return Err(SqlError::at(
                ix.line,
                ix.col,
                format!("index names unknown column '{c}' of table '{}'", t.name),
            ));
        }
    }
    if ix.cols.len() == 1 {
        attach_single(ix, t, notes)
    } else {
        attach_composite(ix, t)
    }
}

fn attach_single(ix: &CreateIndex, t: &mut Table, notes: &mut Vec<String>) -> Result<(), SqlError> {
    let (col, desc) = &ix.cols[0];
    if *desc {
        return Err(SqlError::at(
            ix.line,
            ix.col,
            format!(
                "DESC on the single-column index ({col}) is not compilable \u{2014} a Range index serves both directions; order at read time (ORDER BY {col} DESC in the view / SORT \u{2026} DESC)"
            ),
        ));
    }
    for v in &ix.include {
        if t.column_type(v).is_none() {
            return Err(SqlError::at(
                ix.line,
                ix.col,
                format!("INCLUDE names unknown column '{v}' of table '{}'", t.name),
            ));
        }
    }
    if let Some(n) = &ix.name {
        notes.push(format!(
            "index {n}: kevy names single-column paths by their column \u{2014} it compiles to {}.{col}",
            t.name
        ));
    }
    push_index(
        t,
        Ix { column: col.clone(), unique: ix.unique, values: ix.include.clone() },
        ix.line,
        ix.col,
    )
}

fn attach_composite(ix: &CreateIndex, t: &mut Table) -> Result<(), SqlError> {
    if ix.unique {
        return Err(SqlError::at(
            ix.line,
            ix.col,
            "a multi-column UNIQUE index is not compilable \u{2014} composite paths are Range; enforce the pair app-side (verify-not-enforce, cookbook \u{a7}6) or concatenate it into one column".to_string(),
        ));
    }
    if !ix.include.is_empty() {
        return Err(SqlError::at(
            ix.line,
            ix.col,
            "a multi-column index cannot carry INCLUDE \u{2014} composite paths store no VALUES; put INCLUDE on a single-column index, or extend the column chain".to_string(),
        ));
    }
    if ix.cols.len() > MAX_COMPOSITE_COLS {
        return Err(SqlError::at(ix.line, ix.col, "a composite index supports at most 8 columns"));
    }
    let name = ix.name.clone().unwrap_or_else(|| {
        ix.cols.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>().join("_")
    });
    if t.orderpaths.iter().any(|o| o.name == name) {
        return Err(SqlError::at(ix.line, ix.col, format!("duplicate composite index '{name}'")));
    }
    if t.indexes.iter().any(|e| e.column == name) {
        return Err(SqlError::at(
            ix.line,
            ix.col,
            format!(
                "composite index name '{name}' collides with the single-column index on '{name}' \u{2014} name it explicitly (CREATE INDEX <name> ON \u{2026})"
            ),
        ));
    }
    t.orderpaths.push(OrderPath { name, on: ix.cols.clone() });
    Ok(())
}

/// The `TABLE.DECLARE` argv for one accumulated table — the exact
/// wire shape `kevy_index::parse_table_declare` accepts.
pub(crate) fn declare_argv(t: &Table) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "TABLE.DECLARE".into(),
        t.name.clone(),
        "PREFIX".into(),
        t.prefix(),
        "PK".into(),
        t.pk.clone(),
    ];
    for (name, ty) in &t.columns {
        a.push("COLUMN".into());
        a.push(name.clone());
        a.push(ty.tag().into());
    }
    for ix in &t.indexes {
        a.push("INDEX".into());
        a.push(ix.column.clone());
        a.push(if ix.unique { "unique" } else { "range" }.into());
        if !ix.values.is_empty() {
            a.push("VALUES".into());
            a.extend(ix.values.iter().cloned());
        }
    }
    for op in &t.orderpaths {
        a.push("ORDERPATH".into());
        a.push(op.name.clone());
        a.push("ON".into());
        for (i, (col, desc)) in op.on.iter().enumerate() {
            if i > 0 {
                a.push("THEN".into());
            }
            a.push(col.clone());
            if *desc {
                a.push("DESC".into());
            }
        }
    }
    a
}
