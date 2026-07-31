//! The `TABLE.*` declaration layer.
//!
//! A table is a named, verifiable, catalog-managed DECLARATION that
//! compiles AT DECLARE TIME into the existing IDX primitives — the
//! engine gains no query language, no planner, and enforces no schema
//! at query time (Law 3): a row with a missing column is a row with an
//! absent field, exactly today's NULL semantics. Queries still name
//! their access path explicitly (`IDX.QUERY <table>.<col> …`).
//!
//! [`compile_table`] is the SINGLE implementation both the server and
//! the embedded store call — the IDX.CREATE parity lesson: a
//! hand-mirrored compiler is the shape that drifts, and the dispatch
//! oracle is the net that catches it.

use crate::catalog::{IndexKind, IndexSpec, ValType, ValueSpec};
use crate::composite::{CompositeCol, MAX_COMPOSITE_COLS};

/// One declared secondary index: a column and a scalar kind, plus the
/// stored `VALUES` columns residual FILTER/SORT read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableIndex {
    /// Declared column the index reads.
    pub column: Vec<u8>,
    /// `Range` or `Unique` — nothing else compiles from a table
    /// (aggregates stay a direct `IDX.CREATE KIND agg` declaration).
    pub kind: IndexKind,
    /// Declared columns stored per row (typed from the column decls).
    pub values: Vec<Vec<u8>>,
}

/// One composite-sort path (`ORDERPATH` — cookbook §8 mechanized):
/// compiles to a composite Range index named `<table>.<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderPath {
    /// Path name (the compiled index's suffix).
    pub name: Vec<u8>,
    /// `(column, desc)` in sort-significance order.
    pub on: Vec<(Vec<u8>, bool)>,
}

/// The sliding value-domain window: rows whose window-column value
/// falls behind the moving boundary become eviction candidates for the
/// cold segment tier. Units belong to the caller — the engine never
/// interprets the column's i64 beyond ordering, so a window column can
/// be epoch seconds, epoch millis, a sequence number, anything
/// monotone with data age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSpec {
    /// Declared i64 column the window slides over.
    pub column: Vec<u8>,
    /// Window length, in the column's own units.
    pub span: i64,
    /// Slide granularity, same units: the boundary advances in whole
    /// buckets, and an evicted bucket is a segment.
    pub bucket: i64,
}

/// One declared table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableSpec {
    /// Unique catalog name.
    pub name: Vec<u8>,
    /// Key-prefix domain the table's rows live under.
    pub prefix: Vec<u8>,
    /// Primary-key column (documentation + VERIFY surface; rows are
    /// addressed by their key, exactly as today).
    pub pk: Vec<u8>,
    /// Declared columns with their scalar types, declaration order.
    pub columns: Vec<(Vec<u8>, ValType)>,
    /// Declared secondary indexes.
    pub indexes: Vec<TableIndex>,
    /// Declared composite-sort paths.
    pub orderpaths: Vec<OrderPath>,
    /// Optional sliding hot window (`WINDOW <col> SPAN <n> BUCKET <n>`).
    pub window: Option<WindowSpec>,
}

/// Hard cap on declared tables.
pub const MAX_TABLES: usize = 64;

impl TableSpec {
    /// The declared type of `col`, if declared.
    pub fn column_type(&self, col: &[u8]) -> Option<ValType> {
        self.columns.iter().find(|(n, _)| n == col).map(|(_, t)| *t)
    }

    /// Structural validation — every refusal named. Runs at parse time
    /// AND at catalog admission (a sidecar line re-validates on load).
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("ERR table name must be non-empty".into());
        }
        if self.prefix.is_empty() {
            return Err("ERR PREFIX must be non-empty".into());
        }
        if self.columns.is_empty() {
            return Err("ERR a table needs at least one COLUMN".into());
        }
        self.validate_columns_and_pk()?;
        self.validate_indexes()?;
        self.validate_orderpaths()?;
        self.validate_window()
    }

    /// The window needs an i64 column, positive span/bucket with
    /// bucket <= span, and an access path whose tree tail can answer
    /// max(column) for free: a single-column INDEX on it, or an
    /// ORDERPATH whose FIRST column is it, ascending.
    fn validate_window(&self) -> Result<(), String> {
        let Some(w) = &self.window else { return Ok(()) };
        match self.column_type(&w.column) {
            None => {
                return Err(format!("ERR WINDOW names unknown column '{}'", show(&w.column)));
            }
            Some(ValType::I64) => {}
            Some(_) => return Err("ERR WINDOW column must be i64".into()),
        }
        if w.span <= 0 || w.bucket <= 0 {
            return Err("ERR WINDOW SPAN and BUCKET must be positive".into());
        }
        if w.bucket > w.span {
            return Err("ERR WINDOW BUCKET must not exceed SPAN".into());
        }
        let indexed = self.indexes.iter().any(|ix| ix.column == w.column);
        let leads_path = self
            .orderpaths
            .iter()
            .any(|op| op.on.first().is_some_and(|(c, desc)| c == &w.column && !desc));
        if !indexed && !leads_path {
            return Err(format!(
                "ERR WINDOW needs an access path on '{}' (add INDEX {} range, or lead an                  ORDERPATH with it ascending)",
                show(&w.column),
                show(&w.column)
            ));
        }
        Ok(())
    }

    fn validate_columns_and_pk(&self) -> Result<(), String> {
        for (i, (name, ty)) in self.columns.iter().enumerate() {
            if !matches!(ty, ValType::I64 | ValType::F64 | ValType::Str) {
                return Err("ERR COLUMN type must be i64|f64|str".into());
            }
            if self.columns[..i].iter().any(|(n, _)| n == name) {
                return Err(format!("ERR duplicate COLUMN '{}'", show(name)));
            }
        }
        if self.column_type(&self.pk).is_none() {
            return Err(format!(
                "ERR PK column '{}' is not declared (add COLUMN {} ...)",
                show(&self.pk),
                show(&self.pk)
            ));
        }
        Ok(())
    }

    fn validate_indexes(&self) -> Result<(), String> {
        for (i, ix) in self.indexes.iter().enumerate() {
            if !matches!(ix.kind, IndexKind::Range | IndexKind::Unique) {
                return Err("ERR INDEX kind must be range|unique".into());
            }
            if self.column_type(&ix.column).is_none() {
                return Err(format!("ERR INDEX names unknown column '{}'", show(&ix.column)));
            }
            if self.indexes[..i].iter().any(|p| p.column == ix.column) {
                return Err(format!("ERR duplicate INDEX on column '{}'", show(&ix.column)));
            }
            for v in &ix.values {
                if self.column_type(v).is_none() {
                    return Err(format!("ERR VALUES names unknown column '{}'", show(v)));
                }
            }
        }
        Ok(())
    }

    fn validate_orderpaths(&self) -> Result<(), String> {
        for (i, op) in self.orderpaths.iter().enumerate() {
            if op.on.is_empty() {
                return Err("ERR ORDERPATH needs ON <col>".into());
            }
            if op.on.len() > MAX_COMPOSITE_COLS {
                return Err("ERR ORDERPATH supports at most 8 columns".into());
            }
            if self.orderpaths[..i].iter().any(|p| p.name == op.name) {
                return Err(format!("ERR duplicate ORDERPATH '{}'", show(&op.name)));
            }
            // The compiled names share one namespace: `<table>.<col>`
            // vs `<table>.<orderpath>` colliding would be two indexes
            // with one name — refused here, by name, not downstream.
            if self.indexes.iter().any(|ix| ix.column == op.name) {
                return Err(format!(
                    "ERR ORDERPATH '{}' collides with INDEX '{}'",
                    show(&op.name),
                    show(&op.name)
                ));
            }
            for (col, _) in &op.on {
                if self.column_type(col).is_none() {
                    return Err(format!(
                        "ERR ORDERPATH '{}' names unknown column '{}'",
                        show(&op.name),
                        show(col)
                    ));
                }
            }
        }
        Ok(())
    }
}

pub(crate) use crate::table_sidecar::{spec_from_line, spec_to_line};

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `<table>.<suffix>` — the compiled access-path name.
fn dotted(table: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut n = table.to_vec();
    n.push(b'.');
    n.extend_from_slice(suffix);
    n
}

/// Compile a table into its access paths: each `INDEX col KIND` becomes
/// an IndexSpec named `<table>.<col>` on the table's prefix (FIELD col,
/// TYPE from the column decl, VALUES typed from the column decls); each
/// `ORDERPATH` becomes a composite Range IndexSpec named
/// `<table>.<orderpath>`. Pure — the SINGLE compilation both the server
/// and the embedded store install.
///
/// **Validates first, itself.** The 4.0 shape took "a validated table"
/// on trust and cashed that trust as `expect("validated")` — and the
/// typed embedded face never called `validate()` at all, so a spec
/// whose ORDERPATH named an undeclared column panicked in here, on a
/// consumer's boot path, and restart-looped their container (dogfood
/// F9). An invariant a function needs is one it establishes: admission
/// has exactly one authority now, and it is this signature. The wire
/// path's second validation costs microseconds.
pub fn compile_table(t: &TableSpec) -> Result<Vec<IndexSpec>, String> {
    t.validate()?;
    let col_ty = |col: &[u8]| {
        // Post-validate this is total; the Err arm is the honest form
        // of what `expect` asserted, kept reachable so a validate()
        // gap can never again become a panic.
        t.column_type(col)
            .ok_or_else(|| format!("ERR column '{}' is not declared", show(col)))
    };
    let mut out = Vec::with_capacity(t.indexes.len() + t.orderpaths.len());
    for ix in &t.indexes {
        let ty = col_ty(&ix.column)?;
        let mut spec = IndexSpec::single_field(
            dotted(&t.name, &ix.column),
            t.prefix.clone(),
            ix.column.clone(),
            ty,
            ix.kind,
        );
        spec.values = ix
            .values
            .iter()
            .map(|c| Ok(ValueSpec { name: c.clone(), ty: col_ty(c)? }))
            .collect::<Result<_, String>>()?;
        out.push(spec);
    }
    for op in &t.orderpaths {
        let mut spec = IndexSpec::single_field(
            dotted(&t.name, &op.name),
            t.prefix.clone(),
            op.on[0].0.clone(),
            ValType::Str,
            IndexKind::Range,
        );
        spec.composite = Some(
            op.on
                .iter()
                .map(|(col, desc)| {
                    Ok(CompositeCol { name: col.clone(), ty: col_ty(col)?, desc: *desc })
                })
                .collect::<Result<_, String>>()?,
        );
        out.push(spec);
    }
    Ok(out)
}

/// The table registry (mirrors [`crate::Catalog`]): named specs +
/// sidecar text round-trip. Cap [`MAX_TABLES`].
#[derive(Debug, Clone, Default)]
pub struct TableCatalog {
    specs: Vec<TableSpec>,
}

impl TableCatalog {
    /// Empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register; errors on duplicate / cap / structure.
    pub fn create(&mut self, spec: TableSpec) -> Result<(), String> {
        spec.validate()?;
        if self.specs.len() >= MAX_TABLES {
            return Err("ERR table limit reached (64)".into());
        }
        if self.specs.iter().any(|s| s.name == spec.name) {
            return Err("ERR table already exists".into());
        }
        self.specs.push(spec);
        Ok(())
    }

    /// Drop by name; `false` if absent.
    pub fn drop_table(&mut self, name: &[u8]) -> bool {
        let n = self.specs.len();
        self.specs.retain(|s| s.name != name);
        self.specs.len() != n
    }

    /// Lookup.
    pub fn get(&self, name: &[u8]) -> Option<&TableSpec> {
        self.specs.iter().find(|s| s.name == name)
    }

    /// Declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &TableSpec> {
        self.specs.iter()
    }

    /// Count.
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Empty?
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Sidecar text (one line per table) — same lifecycle genre as the
    /// index/view catalogs.
    pub fn to_sidecar(&self) -> String {
        let mut out = String::from("kevy-table-catalog v1\n");
        for s in &self.specs {
            out.push_str(&spec_to_line(s));
            out.push('\n');
        }
        out
    }

    /// Parse the sidecar text; `None` on malformed input. Every line
    /// re-validates — a spec the validator refuses cannot be smuggled
    /// in through a hand-edited sidecar.
    pub fn from_sidecar(text: &str) -> Option<TableCatalog> {
        let mut lines = text.lines();
        if lines.next()? != "kevy-table-catalog v1" {
            return None;
        }
        let mut c = TableCatalog::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            c.create(spec_from_line(line)?).ok()?;
        }
        Some(c)
    }
}

#[cfg(test)]
#[path = "table_tests.rs"]
mod tests;
