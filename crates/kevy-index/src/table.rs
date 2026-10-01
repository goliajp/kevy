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
use crate::spec::IndexSpec;
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
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 COLUMN at i64 INDEX at range")?;
    /// assert_eq!(t.indexes[0].column, b"at");
    /// assert_eq!(t.compile()?[0].name(), b"t.at", "the column names the compiled index");
    /// # Ok::<(), TableError>(())
    /// ```
    pub column: Vec<u8>,
    /// `Range` or `Unique` — nothing else compiles from a table
    /// (aggregates stay a direct `IDX.CREATE KIND agg` declaration).
    ///
    /// ```
    /// # use kevy_index::{IndexKind, TableError};
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id unique")?;
    /// assert_eq!(t.indexes[0].kind, IndexKind::Unique);
    /// # Ok::<(), TableError>(())
    /// ```
    pub kind: IndexKind,
    /// Declared columns stored per row (typed from the column decls).
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 COLUMN city str INDEX id range VALUES city")?;
    /// assert_eq!(t.indexes[0].values, [b"city".to_vec()]);
    /// assert_eq!(t.compile()?[0].values().len(), 1, "stored alongside each entry");
    /// # Ok::<(), TableError>(())
    /// ```
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
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 ORDERPATH newest ON id DESC")?;
    /// assert_eq!(t.orderpaths[0].name, b"newest");
    /// assert_eq!(t.compile()?[0].name(), b"t.newest");
    /// # Ok::<(), TableError>(())
    /// ```
    pub name: Vec<u8>,
    /// `(column, direction)` in sort-significance order.
    ///
    /// ```
    /// # use kevy_index::{SortOrder, TableError};
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 COLUMN c str ORDERPATH p ON c THEN id DESC")?;
    /// assert_eq!(t.orderpaths[0].on, [(b"c".to_vec(), SortOrder::Asc), (b"id".to_vec(), SortOrder::Desc)]);
    /// # Ok::<(), TableError>(())
    /// ```
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
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE t PREFIX t: PK at COLUMN at i64 INDEX at range WINDOW at SPAN 60 BUCKET 10")?;
    /// assert_eq!(t.window.map(|w| w.column), Some(b"at".to_vec()));
    /// # Ok::<(), TableError>(())
    /// ```
    pub column: Vec<u8>,
    /// Window length, in the column's own units.
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE t PREFIX t: PK at COLUMN at i64 INDEX at range WINDOW at SPAN 60 BUCKET 10")?;
    /// assert_eq!(t.window.map(|w| w.span), Some(60));
    /// # Ok::<(), TableError>(())
    /// ```
    pub span: i64,
    /// Slide granularity, same units: the boundary advances in whole
    /// buckets, and an evicted bucket is a segment.
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE t PREFIX t: PK at COLUMN at i64 INDEX at range WINDOW at SPAN 60 BUCKET 10")?;
    /// assert_eq!(t.window.map(|w| w.bucket), Some(10));
    /// # Ok::<(), TableError>(())
    /// ```
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
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range")?;
    /// assert_eq!(t.name, b"users");
    /// assert_eq!(t.compile()?[0].name(), b"users.age", "compiled paths are named under the table");
    /// # Ok::<(), TableError>(())
    /// ```
    pub name: Vec<u8>,
    /// Key-prefix domain the table's rows live under.
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range")?;
    /// assert_eq!(t.prefix, b"user:");
    /// assert_eq!(t.compile()?[0].prefix(), b"user:", "every path indexes the rows under it");
    /// # Ok::<(), TableError>(())
    /// ```
    pub prefix: Vec<u8>,
    /// Primary-key column (documentation + VERIFY surface; rows are
    /// addressed by their key, exactly as today).
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range")?;
    /// assert_eq!(t.pk, b"id");
    /// # Ok::<(), TableError>(())
    /// ```
    pub pk: Vec<u8>,
    /// Declared columns with their scalar types, declaration order.
    ///
    /// ```
    /// # use kevy_index::{TableError, ValType};
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range")?;
    /// assert_eq!(t.columns, [(b"id".to_vec(), ValType::I64), (b"age".to_vec(), ValType::I64)]);
    /// assert_eq!(t.column_type(b"age"), Some(ValType::I64));
    /// assert_eq!(t.column_type(b"city"), None);
    /// # Ok::<(), TableError>(())
    /// ```
    pub columns: Vec<(Vec<u8>, ValType)>,
    /// Declared secondary indexes.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, TableError, TableIndex};
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range")?;
    /// assert_eq!(t.indexes, [TableIndex::new("age", IndexKind::Range)]);
    /// # Ok::<(), TableError>(())
    /// ```
    pub indexes: Vec<TableIndex>,
    /// Declared composite-sort paths.
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let t = declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range ORDERPATH oldest ON age DESC THEN id")?;
    /// assert_eq!(t.orderpaths.len(), 1);
    /// assert_eq!(t.compile()?.len(), 2, "one index and one order path");
    /// # Ok::<(), TableError>(())
    /// ```
    pub orderpaths: Vec<OrderPath>,
    /// Optional sliding hot window (`WINDOW <col> SPAN <n> BUCKET <n>`).
    ///
    /// ```
    /// # use kevy_index::{TableError, WindowSpec};
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// assert_eq!(declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range")?.window, None);
    /// let t = declare("TABLE.DECLARE ev PREFIX ev: PK at COLUMN at i64 INDEX at range WINDOW at SPAN 3600 BUCKET 60")?;
    /// assert_eq!(t.window, Some(WindowSpec::new("at", 3600, 60)));
    /// # Ok::<(), TableError>(())
    /// ```
    pub window: Option<WindowSpec>,
    /// `AUTODECLARE <n>`: how many paths the engine may declare for
    /// this table from observed refusals (0 = the loop is off, the
    /// default). Building is addition-safe — the worst case is
    /// bounded wasted memory; dropping stays a human act.
    ///
    /// ```
    /// # use kevy_index::TableError;
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// assert_eq!(declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range")?.autodeclare, 0, "off unless declared");
    /// assert_eq!(declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range AUTODECLARE 3")?.autodeclare, 3);
    /// # Ok::<(), TableError>(())
    /// ```
    pub autodeclare: usize,
    /// The paths the auto loop has declared, in declaration order —
    /// its spent budget, and the `auto` marker IDX.LIST shows.
    /// Runtime provenance, not declaration intent: equality checks
    /// that answer "is this the same declaration?" must ignore it
    /// (see [`Self::sans_auto`]).
    ///
    /// ```
    /// # use kevy_index::{IndexKind, TableError, TableIndex};
    /// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let declared = declare("TABLE.DECLARE users PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range AUTODECLARE 2")?;
    /// // the engine added an index on id, and records that it did
    /// let mut grown = declared.clone();
    /// grown.indexes.push(TableIndex::new("id", IndexKind::Unique));
    /// grown.auto_added.push(b"users.id".to_vec());
    /// assert_eq!(grown.sans_auto(), declared, "what the human declared is unchanged");
    /// # Ok::<(), TableError>(())
    /// ```
    pub auto_added: Vec<Vec<u8>>,
}

/// Hard cap on declared tables.
///
/// ```
/// use kevy_index::{CatalogError, Declared, MAX_TABLES, TableCatalog};
/// # let declare = |s: &str| kevy_index::parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
/// let mut tables = TableCatalog::new();
/// for i in 0..MAX_TABLES {
///     tables.create(declare(&format!("TABLE.DECLARE t{i} PREFIX t{i}: PK id COLUMN id i64"))?)?;
/// }
/// let one_more = declare("TABLE.DECLARE extra PREFIX x: PK id COLUMN id i64")?;
/// assert_eq!(tables.create(one_more), Err(CatalogError::Full(Declared::Table)));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
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
}

pub use crate::table_catalog::TableCatalog;

/// `<table>.<suffix>` — the compiled access-path name.
pub(crate) fn dotted(table: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut n = table.to_vec();
    n.push(b'.');
    n.extend_from_slice(suffix);
    n
}

#[path = "table_compile.rs"]
mod compile;
use compile::compile_table;

#[cfg(test)]
#[path = "table_tests.rs"]
mod tests;
