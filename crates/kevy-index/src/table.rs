//! The `TABLE.*` declaration layer.
//!
//! A table is a named, verifiable, catalog-managed DECLARATION that
//! compiles AT DECLARE TIME into the existing IDX primitives — the
//! engine gains no query language, no planner, and enforces no schema
//! at query time (Law 3): a row with a missing column is a row with an
//! absent field, exactly today's NULL semantics. Queries still name
//! their access path explicitly (`IDX.QUERY <table>.<col> …`).
//!
//! [`TableSpec::compile`] is the SINGLE implementation both the server and
//! the embedded store call — the IDX.CREATE parity lesson: a
//! hand-mirrored compiler is the shape that drifts, and the dispatch
//! oracle is the net that catches it.

use crate::catalog::{IndexKind, ValType};
use crate::composite::{CompositeCol, MAX_COMPOSITE_COLS};
use crate::spec::IndexSpec;
use crate::spec_parts::ValueSpec;
use crate::table_error::TableError;
use kevy_text::SortOrder;

/// One declared secondary index: a column and a scalar kind, plus the
/// stored `VALUES` columns residual FILTER/SORT read.
///
/// ```
/// use kevy_index::{IndexKind, TableIndex};
/// let mut ix = TableIndex::new("at", IndexKind::Range);
/// ix.values.push(b"city".to_vec());
/// assert_eq!((ix.column.as_slice(), ix.values.len()), (&b"at"[..], 1));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TableIndex {
    /// Declared column the index reads.
    pub column: Vec<u8>,
    /// `Range` or `Unique` — nothing else compiles from a table
    /// (aggregates stay a direct `IDX.CREATE KIND agg` declaration).
    pub kind: IndexKind,
    /// Declared columns stored per row (typed from the column decls).
    pub values: Vec<Vec<u8>>,
}

impl TableIndex {
    /// An index of `kind` on `column`, storing no `VALUES`.
    ///
    /// ```
    /// use kevy_index::{IndexKind, TableIndex};
    /// assert!(TableIndex::new("id", IndexKind::Unique).values.is_empty());
    /// ```
    pub fn new(column: impl Into<Vec<u8>>, kind: IndexKind) -> TableIndex {
        TableIndex { column: column.into(), kind, values: Vec::new() }
    }
}

/// One composite-sort path (`ORDERPATH` — cookbook §8 mechanized):
/// compiles to a composite Range index named `<table>.<name>`.
///
/// ```
/// use kevy_index::{OrderPath, SortOrder};
/// let p = OrderPath::new("recent", vec![(b"at".to_vec(), SortOrder::Desc)]);
/// assert_eq!(p.on[0].1, SortOrder::Desc);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct OrderPath {
    /// Path name (the compiled index's suffix).
    pub name: Vec<u8>,
    /// `(column, direction)` in sort-significance order.
    pub on: Vec<(Vec<u8>, SortOrder)>,
}

impl OrderPath {
    /// Path `name` over `on`, most significant column first.
    ///
    /// ```
    /// use kevy_index::{OrderPath, SortOrder};
    /// let p = OrderPath::new("by_city", vec![(b"city".to_vec(), SortOrder::Asc)]);
    /// assert_eq!(p.name, b"by_city");
    /// ```
    pub fn new(name: impl Into<Vec<u8>>, on: Vec<(Vec<u8>, SortOrder)>) -> OrderPath {
        OrderPath { name: name.into(), on }
    }

    /// Whether this path's first column is `column`, ascending — the
    /// shape whose tree prefix below a boundary is the out-of-window batch.
    pub(crate) fn led_ascending_by(&self, column: &[u8]) -> bool {
        self.on.first().is_some_and(|(c, o)| c == column && *o == SortOrder::Asc)
    }
}

/// The sliding value-domain window: rows whose window-column value
/// falls behind the moving boundary become eviction candidates for the
/// cold segment tier. Units belong to the caller — the engine never
/// interprets the column's i64 beyond ordering, so a window column can
/// be epoch seconds, epoch millis, a sequence number, anything
/// monotone with data age.
///
/// ```
/// use kevy_index::WindowSpec;
/// let w = WindowSpec::new("at", 86_400, 3_600);
/// assert_eq!((w.span, w.bucket), (86_400, 3_600));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct WindowSpec {
    /// Declared i64 column the window slides over.
    pub column: Vec<u8>,
    /// Window length, in the column's own units.
    pub span: i64,
    /// Slide granularity, same units: the boundary advances in whole
    /// buckets, and an evicted bucket is a segment.
    pub bucket: i64,
}

impl WindowSpec {
    /// A window of `span` over i64 column `column`, sliding a `bucket`
    /// at a time (same units as the column).
    ///
    /// ```
    /// assert_eq!(kevy_index::WindowSpec::new("seq", 100, 10).column, b"seq");
    /// ```
    pub fn new(column: impl Into<Vec<u8>>, span: i64, bucket: i64) -> WindowSpec {
        WindowSpec { column: column.into(), span, bucket }
    }
}

/// One declared table.
///
/// A declaration callers assemble (or [`parse_table_declare`](crate::parse_table_declare)
/// parses) and the auto loop edits: its rules span fields — an index
/// must name a declared column, a window needs an ascending access path —
/// so they are checked where a table is admitted ([`TableSpec::validate`],
/// run by [`TableCatalog::create`] and [`TableSpec::compile`]), not per field.
///
/// ```
/// use kevy_index::{IndexKind, TableIndex, TableSpec, ValType};
/// let mut t = TableSpec::default();
/// t.name = b"t".to_vec();
/// t.prefix = b"t:".to_vec();
/// t.pk = b"id".to_vec();
/// t.columns = vec![(b"id".to_vec(), ValType::I64), (b"at".to_vec(), ValType::I64)];
/// t.indexes.push(TableIndex::new("at", IndexKind::Range));
/// assert_eq!(t.compile()?[0].name(), b"t.at");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
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
    /// `AUTODECLARE <n>`: how many paths the engine may declare for
    /// this table from observed refusals (0 = the loop is off, the
    /// default). Building is addition-safe — the worst case is
    /// bounded wasted memory; dropping stays a human act.
    pub autodeclare: usize,
    /// The paths the auto loop has declared, in declaration order —
    /// its spent budget, and the `auto` marker IDX.LIST shows.
    /// Runtime provenance, not declaration intent: equality checks
    /// that answer "is this the same declaration?" must ignore it
    /// (see [`Self::sans_auto`]).
    pub auto_added: Vec<Vec<u8>>,
}

/// Hard cap on declared tables.
pub const MAX_TABLES: usize = 64;

impl TableSpec {
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
    ///
    /// ```
    /// use kevy_index::parse_table_declare;
    /// let t = parse_table_declare(&[
    ///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
    ///     b"COLUMN", b"at", b"i64", b"INDEX", b"at", b"range",
    /// ])?;
    /// let compiled = t.compile()?;
    /// assert_eq!(compiled[0].name(), b"t.at");
    /// # Ok::<(), kevy_index::TableError>(())
    /// ```
    pub fn compile(&self) -> Result<Vec<IndexSpec>, TableError> {
        compile_table(self)
    }

    /// The declared type of `col`, if declared.
    pub fn column_type(&self, col: &[u8]) -> Option<ValType> {
        self.columns.iter().find(|(n, _)| n == col).map(|(_, t)| *t)
    }

    /// This declaration with the auto loop's runtime additions
    /// removed — what the human actually declared. `ENSURE`-style
    /// "is this the same declaration?" comparisons go through here,
    /// so paths the engine added never read as drift. Entries are
    /// path names (a whole auto index/orderpath) or `path#field` (an
    /// auto VALUES column on a human-declared index).
    #[must_use]
    pub fn sans_auto(&self) -> TableSpec {
        let mut s = self.clone();
        let auto = std::mem::take(&mut s.auto_added);
        let suffix_of = |entry: &[u8]| -> Option<Vec<u8>> {
            let e = entry.split(|&b| b == b'#').next()?;
            let dot = e.iter().position(|&b| b == b'.')?;
            Some(e[dot + 1..].to_vec())
        };
        for entry in &auto {
            if let Some(pos) = entry.iter().position(|&b| b == b'#') {
                let field = &entry[pos + 1..];
                if let Some(sfx) = suffix_of(entry)
                    && let Some(ix) = s.indexes.iter_mut().find(|ix| ix.column == sfx)
                {
                    ix.values.retain(|v| v != field);
                }
            } else if let Some(sfx) = suffix_of(entry) {
                s.indexes.retain(|ix| ix.column != sfx);
                s.orderpaths.retain(|op| op.name != sfx);
            }
        }
        s
    }

    /// Structural validation — every refusal named. Runs at parse time
    /// AND at catalog admission (a sidecar line re-validates on load).
    pub fn validate(&self) -> Result<(), TableError> {
        if self.name.is_empty() {
            return Err(TableError::EmptyName);
        }
        if self.prefix.is_empty() {
            return Err(TableError::EmptyPrefix);
        }
        if self.columns.is_empty() {
            return Err(TableError::NoColumns);
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
    fn validate_window(&self) -> Result<(), TableError> {
        let Some(w) = &self.window else { return Ok(()) };
        match self.column_type(&w.column) {
            None => return Err(TableError::WindowUnknownColumn(w.column.clone())),
            Some(ValType::I64) => {}
            Some(_) => return Err(TableError::WindowColumnType),
        }
        if w.span <= 0 || w.bucket <= 0 {
            return Err(TableError::WindowNotPositive);
        }
        if w.bucket > w.span {
            return Err(TableError::WindowBucketExceedsSpan);
        }
        let indexed = self.indexes.iter().any(|ix| ix.column == w.column);
        let leads_path = self.orderpaths.iter().any(|op| op.led_ascending_by(&w.column));
        if !indexed && !leads_path {
            return Err(TableError::WindowNeedsPath(w.column.clone()));
        }
        Ok(())
    }

    fn validate_columns_and_pk(&self) -> Result<(), TableError> {
        for (i, (name, ty)) in self.columns.iter().enumerate() {
            if !matches!(ty, ValType::I64 | ValType::F64 | ValType::Str) {
                return Err(TableError::ColumnType);
            }
            if self.columns[..i].iter().any(|(n, _)| n == name) {
                return Err(TableError::DuplicateColumn(name.clone()));
            }
        }
        if self.column_type(&self.pk).is_none() {
            return Err(TableError::PkUndeclared(self.pk.clone()));
        }
        Ok(())
    }

    fn validate_indexes(&self) -> Result<(), TableError> {
        for (i, ix) in self.indexes.iter().enumerate() {
            if !matches!(ix.kind, IndexKind::Range | IndexKind::Unique) {
                return Err(TableError::IndexKind);
            }
            if self.column_type(&ix.column).is_none() {
                return Err(TableError::IndexUnknownColumn(ix.column.clone()));
            }
            if self.indexes[..i].iter().any(|p| p.column == ix.column) {
                return Err(TableError::DuplicateIndex(ix.column.clone()));
            }
            for v in &ix.values {
                if self.column_type(v).is_none() {
                    return Err(TableError::ValuesUnknownColumn(v.clone()));
                }
            }
        }
        Ok(())
    }

    fn validate_orderpaths(&self) -> Result<(), TableError> {
        for (i, op) in self.orderpaths.iter().enumerate() {
            if op.on.is_empty() {
                return Err(TableError::OrderpathNeedsOn);
            }
            if op.on.len() > MAX_COMPOSITE_COLS {
                return Err(TableError::OrderpathTooManyColumns);
            }
            if self.orderpaths[..i].iter().any(|p| p.name == op.name) {
                return Err(TableError::DuplicateOrderpath(op.name.clone()));
            }
            // The compiled names share one namespace: `<table>.<col>`
            // vs `<table>.<orderpath>` colliding would be two indexes
            // with one name — refused here, by name, not downstream.
            if self.indexes.iter().any(|ix| ix.column == op.name) {
                return Err(TableError::OrderpathCollides(op.name.clone()));
            }
            for (col, _) in &op.on {
                if self.column_type(col).is_none() {
                    return Err(TableError::OrderpathUnknownColumn {
                        path: op.name.clone(),
                        column: col.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

pub use crate::table_catalog::TableCatalog;

/// `<table>.<suffix>` — the compiled access-path name.
pub(crate) fn dotted(table: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut n = table.to_vec();
    n.push(b'.');
    n.extend_from_slice(suffix);
    n
}

pub(crate) fn compile_table(t: &TableSpec) -> Result<Vec<IndexSpec>, TableError> {
    t.validate()?;
    let col_ty = |col: &[u8]| {
        // Post-validate this is total; the Err arm is the honest form
        // of what `expect` asserted, kept reachable so a validate()
        // gap can never again become a panic.
        t.column_type(col).ok_or_else(|| TableError::ColumnUndeclared(col.to_vec()))
    };
    let mut out = Vec::with_capacity(t.indexes.len() + t.orderpaths.len());
    for ix in &t.indexes {
        let ty = col_ty(&ix.column)?;
        let values = ix
            .values
            .iter()
            .map(|c| Ok(ValueSpec::new(c.clone()).with_type(col_ty(c)?)))
            .collect::<Result<_, TableError>>()?;
        let spec = IndexSpec::builder(dotted(&t.name, &ix.column), t.prefix.clone(), ix.kind, ty)
            .with_field(ix.column.clone())
            .with_values(values);
        out.push(spec.build()?);
    }
    for op in &t.orderpaths {
        let cols = op
            .on
            .iter()
            .map(|(col, order)| Ok(CompositeCol::new(col.clone(), col_ty(col)?).with_order(*order)))
            .collect::<Result<_, TableError>>()?;
        let spec = IndexSpec::builder(
            dotted(&t.name, &op.name),
            t.prefix.clone(),
            IndexKind::Range,
            ValType::Str,
        )
        .with_field(op.on[0].0.clone())
        .with_composite(cols);
        out.push(spec.build()?);
    }
    Ok(out)
}

#[cfg(test)]
#[path = "table_tests.rs"]
mod tests;
