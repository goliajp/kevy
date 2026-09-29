//! Why a table declaration or a composite `WHERE` was refused.

use std::fmt;

use crate::error::SpecError;

fn show(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

/// Which bound of a `WINDOW` clause.
///
/// ```
/// assert_eq!(kevy_index::WindowBound::Span.as_str(), "SPAN");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WindowBound {
    /// `SPAN n`.
    ///
    /// ```
    /// use kevy_index::{TableError, WindowBound, parse_table_declare};
    /// let argv = "TABLE.DECLARE t PREFIX t: PK at COLUMN at i64 INDEX at range WINDOW at SPAN 1d BUCKET 1";
    /// let argv: Vec<&[u8]> = argv.split(' ').map(str::as_bytes).collect();
    /// assert_eq!(parse_table_declare(&argv), Err(TableError::WindowNotInteger(WindowBound::Span)));
    /// ```
    Span,
    /// `BUCKET n`.
    ///
    /// ```
    /// use kevy_index::{TableError, WindowBound, parse_table_declare};
    /// let argv = "TABLE.DECLARE t PREFIX t: PK at COLUMN at i64 INDEX at range WINDOW at SPAN 10 BUCKET x";
    /// let argv: Vec<&[u8]> = argv.split(' ').map(str::as_bytes).collect();
    /// assert_eq!(parse_table_declare(&argv), Err(TableError::WindowNotInteger(WindowBound::Bucket)));
    /// ```
    Bucket,
}

impl WindowBound {
    /// The clause keyword.
    ///
    /// ```
    /// assert_eq!(kevy_index::WindowBound::Bucket.as_str(), "BUCKET");
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Span => "SPAN",
            Self::Bucket => "BUCKET",
        }
    }
}

/// Why a `TABLE.DECLARE` did not parse, a [`crate::TableSpec`] did not
/// validate, or it did not compile to its indexes. Names are the bytes as
/// declared.
///
/// ```
/// use kevy_index::{TableError, parse_table_declare};
///
/// let e = parse_table_declare(&[
///     b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64",
///     b"COLUMN", b"id", b"str",
/// ])
/// .unwrap_err();
/// assert_eq!(e, TableError::DuplicateColumn(b"id".to_vec()));
/// assert_eq!(e.to_wire(), "ERR duplicate COLUMN 'id'");
/// ```
///
/// The variants' examples split a declaration on spaces into its argv.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TableError {
    /// The argv does not have the declaration's shape; the reply is the
    /// usage line ([`crate::TABLE_DECLARE_USAGE`]).
    ///
    /// ```
    /// # use kevy_index::{TABLE_DECLARE_USAGE, TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t:").unwrap_err();
    /// assert_eq!((e.clone(), e.to_wire()), (TableError::Usage, TABLE_DECLARE_USAGE.to_string()));
    /// ```
    Usage,
    /// `SPLIT AT` with no values.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range GLOBAL SPLIT AT");
    /// assert_eq!(e, Err(TableError::SplitAtEmpty));
    /// ```
    SplitAtEmpty,
    /// `VALUES` with no columns.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range VALUES");
    /// assert_eq!(e, Err(TableError::ValuesEmpty));
    /// ```
    ValuesEmpty,
    /// A second `AUTODECLARE` clause.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 AUTODECLARE 2 AUTODECLARE 3");
    /// assert_eq!(e, Err(TableError::DuplicateAutodeclare));
    /// ```
    DuplicateAutodeclare,
    /// `AUTODECLARE` without a positive integer.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 AUTODECLARE 0");
    /// assert_eq!(e, Err(TableError::AutodeclareNotPositive));
    /// ```
    AutodeclareNotPositive,
    /// A second `WINDOW` clause.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let w = "WINDOW id SPAN 10 BUCKET 1";
    /// let e = declare(&format!("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range {w} {w}"));
    /// assert_eq!(e, Err(TableError::DuplicateWindow));
    /// ```
    DuplicateWindow,
    /// A `WINDOW` bound that is not an integer.
    ///
    /// ```
    /// # use kevy_index::{TableError, WindowBound, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 WINDOW id SPAN 1.5 BUCKET 1");
    /// assert_eq!(e.unwrap_err().to_wire(), "ERR WINDOW SPAN must be an integer");
    /// ```
    WindowNotInteger(WindowBound),
    /// A `GLOBAL` path where every path is local (the embedded store).
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range GLOBAL");
    /// assert_eq!(e, Err(TableError::GlobalNotHere));
    /// ```
    GlobalNotHere,
    /// A `GLOBAL` path on a windowed table: eviction moves a row's entries
    /// out on the row's own shard, where a global path's entries are not.
    ///
    /// ```
    /// use kevy_index::{TableError, parse_table_declare_partitioned};
    /// let argv = "TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range GLOBAL WINDOW id SPAN 10 BUCKET 1";
    /// let argv: Vec<&[u8]> = argv.split(' ').map(str::as_bytes).collect();
    /// assert_eq!(parse_table_declare_partitioned(&argv), Err(TableError::GlobalWindowed));
    /// ```
    GlobalWindowed,
    /// An empty table name.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let mut t = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64")?;
    /// t.name.clear();
    /// assert_eq!(t.validate(), Err(TableError::EmptyName));
    /// # Ok::<(), TableError>(())
    /// ```
    EmptyName,
    /// An empty `PREFIX`.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let mut t = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64")?;
    /// t.prefix.clear();
    /// assert_eq!(t.validate(), Err(TableError::EmptyPrefix));
    /// # Ok::<(), TableError>(())
    /// ```
    EmptyPrefix,
    /// No `COLUMN`.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let mut t = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64")?;
    /// t.columns.clear();
    /// assert_eq!(t.validate(), Err(TableError::NoColumns));
    /// # Ok::<(), TableError>(())
    /// ```
    NoColumns,
    /// A column type other than i64, f64 or str.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id bool");
    /// assert_eq!(e, Err(TableError::ColumnType));
    /// ```
    ColumnType,
    /// A column declared twice.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 COLUMN at i64 COLUMN at f64");
    /// assert_eq!(e, Err(TableError::DuplicateColumn(b"at".to_vec())));
    /// ```
    DuplicateColumn(Vec<u8>),
    /// A `PK` naming no declared column.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK uid COLUMN id i64").unwrap_err();
    /// assert_eq!(e, TableError::PkUndeclared(b"uid".to_vec()));
    /// assert_eq!(e.to_wire(), "ERR PK column 'uid' is not declared (add COLUMN uid ...)");
    /// ```
    PkUndeclared(Vec<u8>),
    /// An `INDEX` kind other than range or unique.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id text");
    /// assert_eq!(e, Err(TableError::IndexKind));
    /// ```
    IndexKind,
    /// An `INDEX` on no declared column.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX age range");
    /// assert_eq!(e, Err(TableError::IndexUnknownColumn(b"age".to_vec())));
    /// ```
    IndexUnknownColumn(Vec<u8>),
    /// Two `INDEX` clauses on one column.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range INDEX id unique");
    /// assert_eq!(e, Err(TableError::DuplicateIndex(b"id".to_vec())));
    /// ```
    DuplicateIndex(Vec<u8>),
    /// A `VALUES` naming no declared column.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range VALUES city");
    /// assert_eq!(e, Err(TableError::ValuesUnknownColumn(b"city".to_vec())));
    /// ```
    ValuesUnknownColumn(Vec<u8>),
    /// An `ORDERPATH` without `ON <col>`.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 ORDERPATH recent BY id");
    /// assert_eq!(e, Err(TableError::OrderpathNeedsOn));
    /// ```
    OrderpathNeedsOn,
    /// An `ORDERPATH` over more than [`crate::MAX_COMPOSITE_COLS`]
    /// columns.
    ///
    /// ```
    /// # use kevy_index::{MAX_COMPOSITE_COLS, OrderPath, SortOrder, TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let mut t = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64")?;
    /// let on = vec![(b"id".to_vec(), SortOrder::Asc); MAX_COMPOSITE_COLS + 1];
    /// t.orderpaths.push(OrderPath::new("wide", on));
    /// assert_eq!(t.validate(), Err(TableError::OrderpathTooManyColumns));
    /// # Ok::<(), TableError>(())
    /// ```
    OrderpathTooManyColumns,
    /// Two `ORDERPATH` clauses of one name.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 ORDERPATH p ON id ORDERPATH p ON id DESC");
    /// assert_eq!(e, Err(TableError::DuplicateOrderpath(b"p".to_vec())));
    /// ```
    DuplicateOrderpath(Vec<u8>),
    /// An `ORDERPATH` named as an `INDEX` column: both compile to one
    /// index name.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// // both would compile to the index `t.id`
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range ORDERPATH id ON id DESC");
    /// assert_eq!(e, Err(TableError::OrderpathCollides(b"id".to_vec())));
    /// ```
    OrderpathCollides(Vec<u8>),
    /// An `ORDERPATH` over no declared column.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 ORDERPATH recent ON at DESC").unwrap_err();
    /// assert_eq!(e.to_wire(), "ERR ORDERPATH 'recent' names unknown column 'at'");
    /// ```
    OrderpathUnknownColumn {
        /// The order path.
        ///
        /// ```
        /// # use kevy_index::{TableError, parse_table_declare};
        /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
        /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 ORDERPATH p ON id THEN x");
        /// assert!(matches!(e, Err(TableError::OrderpathUnknownColumn { path, .. }) if path == b"p"));
        /// ```
        path: Vec<u8>,
        /// The column it names.
        ///
        /// ```
        /// # use kevy_index::{TableError, parse_table_declare};
        /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
        /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 ORDERPATH p ON id THEN x");
        /// assert!(matches!(e, Err(TableError::OrderpathUnknownColumn { column, .. }) if column == b"x"));
        /// ```
        column: Vec<u8>,
    },
    /// A `WINDOW` on no declared column.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 WINDOW at SPAN 10 BUCKET 1");
    /// assert_eq!(e, Err(TableError::WindowUnknownColumn(b"at".to_vec())));
    /// ```
    WindowUnknownColumn(Vec<u8>),
    /// A `WINDOW` on a column that is not i64.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 COLUMN at f64 WINDOW at SPAN 10 BUCKET 1");
    /// assert_eq!(e, Err(TableError::WindowColumnType));
    /// ```
    WindowColumnType,
    /// A `WINDOW` whose span or bucket is not positive.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range WINDOW id SPAN 0 BUCKET 0");
    /// assert_eq!(e, Err(TableError::WindowNotPositive));
    /// ```
    WindowNotPositive,
    /// A `WINDOW` whose bucket exceeds its span.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 INDEX id range WINDOW id SPAN 1 BUCKET 10");
    /// assert_eq!(e, Err(TableError::WindowBucketExceedsSpan));
    /// ```
    WindowBucketExceedsSpan,
    /// A `WINDOW` column no `INDEX` or ascending-led `ORDERPATH` reads.
    ///
    /// ```
    /// # use kevy_index::{TableError, parse_table_declare};
    /// # let declare = |s: &str| parse_table_declare(&s.split(' ').map(str::as_bytes).collect::<Vec<_>>());
    /// // a descending path cannot find the oldest rows at its head
    /// let e = declare("TABLE.DECLARE t PREFIX t: PK id COLUMN id i64 ORDERPATH p ON id DESC WINDOW id SPAN 10 BUCKET 1");
    /// assert_eq!(e, Err(TableError::WindowNeedsPath(b"id".to_vec())));
    /// ```
    WindowNeedsPath(Vec<u8>),
    /// Compiling met a column the declaration does not declare.
    ///
    /// [`crate::TableSpec::compile`] validates before it compiles, so a
    /// spec that reaches this refusal is one validation missed; the variant
    /// keeps that a named error rather than a panic.
    ///
    /// ```
    /// use kevy_index::TableError;
    /// let e = TableError::ColumnUndeclared(b"at".to_vec());
    /// assert_eq!(e.to_wire(), "ERR column 'at' is not declared");
    /// ```
    ColumnUndeclared(Vec<u8>),
    /// A compiled index refused its own parts.
    ///
    /// ```
    /// use kevy_index::{SpecError, TableError};
    /// use std::error::Error;
    /// let e = TableError::from(SpecError::NoFields);
    /// assert_eq!(e, TableError::Spec(SpecError::NoFields));
    /// assert_eq!(e.to_wire(), "ERR index needs at least one field");
    /// assert!(e.source().is_some(), "the index's own refusal is the source");
    /// ```
    Spec(SpecError),
}

impl TableError {
    /// The refusal as the wire words it.
    ///
    /// ```
    /// assert_eq!(kevy_index::TableError::NoColumns.to_wire(), "ERR a table needs at least one COLUMN");
    /// ```
    pub fn to_wire(&self) -> String {
        format!("ERR {self}")
    }
}

impl fmt::Display for TableError {
    // LOC-WAIVER: a pure match table — one wire message per refusal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str(&crate::TABLE_DECLARE_USAGE["ERR ".len()..]),
            Self::SplitAtEmpty => f.write_str("SPLIT AT needs at least one value"),
            Self::ValuesEmpty => f.write_str("VALUES needs at least one column"),
            Self::DuplicateAutodeclare => f.write_str("duplicate AUTODECLARE clause"),
            Self::AutodeclareNotPositive => f.write_str("AUTODECLARE needs a positive integer"),
            Self::DuplicateWindow => f.write_str("duplicate WINDOW clause"),
            Self::WindowNotInteger(b) => write!(f, "WINDOW {} must be an integer", b.as_str()),
            Self::GlobalNotHere => {
                f.write_str("GLOBAL is a server feature; an embedded store's paths are local")
            }
            Self::GlobalWindowed => f.write_str(
                "GLOBAL cannot apply to a windowed table's paths: eviction moves a row's entries \
                 out on the row's own shard",
            ),
            Self::EmptyName => f.write_str("table name must be non-empty"),
            Self::EmptyPrefix => f.write_str("PREFIX must be non-empty"),
            Self::NoColumns => f.write_str("a table needs at least one COLUMN"),
            Self::ColumnType => f.write_str("COLUMN type must be i64|f64|str"),
            Self::DuplicateColumn(c) => write!(f, "duplicate COLUMN '{}'", show(c)),
            Self::PkUndeclared(c) => {
                write!(f, "PK column '{}' is not declared (add COLUMN {} ...)", show(c), show(c))
            }
            Self::IndexKind => f.write_str("INDEX kind must be range|unique"),
            Self::IndexUnknownColumn(c) => write!(f, "INDEX names unknown column '{}'", show(c)),
            Self::DuplicateIndex(c) => write!(f, "duplicate INDEX on column '{}'", show(c)),
            Self::ValuesUnknownColumn(c) => write!(f, "VALUES names unknown column '{}'", show(c)),
            Self::OrderpathNeedsOn => f.write_str("ORDERPATH needs ON <col>"),
            Self::OrderpathTooManyColumns => f.write_str("ORDERPATH supports at most 8 columns"),
            Self::DuplicateOrderpath(p) => write!(f, "duplicate ORDERPATH '{}'", show(p)),
            Self::OrderpathCollides(p) => {
                write!(f, "ORDERPATH '{}' collides with INDEX '{}'", show(p), show(p))
            }
            Self::OrderpathUnknownColumn { path, column } => {
                write!(f, "ORDERPATH '{}' names unknown column '{}'", show(path), show(column))
            }
            Self::WindowUnknownColumn(c) => write!(f, "WINDOW names unknown column '{}'", show(c)),
            Self::WindowColumnType => f.write_str("WINDOW column must be i64"),
            Self::WindowNotPositive => f.write_str("WINDOW SPAN and BUCKET must be positive"),
            Self::WindowBucketExceedsSpan => f.write_str("WINDOW BUCKET must not exceed SPAN"),
            Self::WindowNeedsPath(c) => write!(
                f,
                "WINDOW needs an access path on '{}' (add INDEX {} range, or lead an ORDERPATH \
                 with it ascending)",
                show(c),
                show(c)
            ),
            Self::ColumnUndeclared(c) => write!(f, "column '{}' is not declared", show(c)),
            Self::Spec(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spec(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SpecError> for TableError {
    fn from(e: SpecError) -> Self {
        Self::Spec(e)
    }
}

#[path = "where_error.rs"]
mod where_error;
pub use where_error::WhereError;
