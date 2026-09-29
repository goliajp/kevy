//! Why a table declaration or a composite `WHERE` was refused.

use std::fmt;

use crate::catalog::ValType;
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
    Span,
    /// `BUCKET n`.
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TableError {
    /// The argv does not have the declaration's shape; the reply is the
    /// usage line ([`crate::TABLE_DECLARE_USAGE`]).
    Usage,
    /// `SPLIT AT` with no values.
    SplitAtEmpty,
    /// `VALUES` with no columns.
    ValuesEmpty,
    /// A second `AUTODECLARE` clause.
    DuplicateAutodeclare,
    /// `AUTODECLARE` without a positive integer.
    AutodeclareNotPositive,
    /// A second `WINDOW` clause.
    DuplicateWindow,
    /// A `WINDOW` bound that is not an integer.
    WindowNotInteger(WindowBound),
    /// A `GLOBAL` path where every path is local (the embedded store).
    GlobalNotHere,
    /// A `GLOBAL` path on a windowed table: eviction moves a row's entries
    /// out on the row's own shard, where a global path's entries are not.
    GlobalWindowed,
    /// An empty table name.
    EmptyName,
    /// An empty `PREFIX`.
    EmptyPrefix,
    /// No `COLUMN`.
    NoColumns,
    /// A column type other than i64, f64 or str.
    ColumnType,
    /// A column declared twice.
    DuplicateColumn(Vec<u8>),
    /// A `PK` naming no declared column.
    PkUndeclared(Vec<u8>),
    /// An `INDEX` kind other than range or unique.
    IndexKind,
    /// An `INDEX` on no declared column.
    IndexUnknownColumn(Vec<u8>),
    /// Two `INDEX` clauses on one column.
    DuplicateIndex(Vec<u8>),
    /// A `VALUES` naming no declared column.
    ValuesUnknownColumn(Vec<u8>),
    /// An `ORDERPATH` without `ON <col>`.
    OrderpathNeedsOn,
    /// An `ORDERPATH` over more than [`crate::MAX_COMPOSITE_COLS`]
    /// columns.
    OrderpathTooManyColumns,
    /// Two `ORDERPATH` clauses of one name.
    DuplicateOrderpath(Vec<u8>),
    /// An `ORDERPATH` named as an `INDEX` column: both compile to one
    /// index name.
    OrderpathCollides(Vec<u8>),
    /// An `ORDERPATH` over no declared column.
    OrderpathUnknownColumn {
        /// The order path.
        path: Vec<u8>,
        /// The column it names.
        column: Vec<u8>,
    },
    /// A `WINDOW` on no declared column.
    WindowUnknownColumn(Vec<u8>),
    /// A `WINDOW` on a column that is not i64.
    WindowColumnType,
    /// A `WINDOW` whose span or bucket is not positive.
    WindowNotPositive,
    /// A `WINDOW` whose bucket exceeds its span.
    WindowBucketExceedsSpan,
    /// A `WINDOW` column no `INDEX` or ascending-led `ORDERPATH` reads.
    WindowNeedsPath(Vec<u8>),
    /// Compiling met a column the declaration does not declare.
    ColumnUndeclared(Vec<u8>),
    /// A compiled index refused its own parts.
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

/// Why [`crate::composite_bounds`] refused a `WHERE`: the words name the
/// bound and the composite's declared columns, and carry no wire code
/// (the caller prefixes the command and index it answers for).
///
/// ```
/// use kevy_index::{CompositeCol, ValType, WhereError, composite_bounds, parse_where};
///
/// let cols = [CompositeCol::new("a", ValType::I64)];
/// let argv: Vec<Vec<u8>> = ["b", "EQ", "1"].iter().map(|s| s.as_bytes().to_vec()).collect();
/// let (w, _) = parse_where(&argv, 0, |_| false).unwrap();
/// let e = composite_bounds(&cols, &w, 0).unwrap_err();
/// assert!(matches!(e, WhereError::UnknownColumn { .. }));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WhereError {
    /// A bound starting `@` that is not a time expression.
    TimeExpression {
        /// The bound as written.
        bound: Vec<u8>,
        /// The column it bounds.
        column: Vec<u8>,
    },
    /// A bound that is not a value of its column's declared type.
    Value {
        /// The bound as written.
        bound: Vec<u8>,
        /// The column's declared type.
        ty: ValType,
        /// The column it bounds.
        column: Vec<u8>,
    },
    /// A `WHERE` column the composite does not declare.
    UnknownColumn {
        /// The column `WHERE` names.
        column: Vec<u8>,
        /// The composite's columns, in declared order.
        declared: Vec<Vec<u8>>,
    },
    /// `WHERE` columns that are not a leading prefix of the declared
    /// order.
    NotLeadingPrefix {
        /// The composite's columns, in declared order.
        declared: Vec<Vec<u8>>,
    },
}

struct List<'a>(&'a [Vec<u8>]);

impl fmt::Display for List<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(&show(c))?;
        }
        Ok(())
    }
}

impl fmt::Display for WhereError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimeExpression { bound, column } => write!(
                f,
                "WHERE bound '{}' is not a valid time expression for '{}'",
                show(bound),
                show(column)
            ),
            Self::Value { bound, ty, column } => write!(
                f,
                "WHERE bound '{}' is not a valid {}, which is how this composite declares '{}'",
                show(bound),
                ty.tag(),
                show(column)
            ),
            Self::UnknownColumn { column, declared } => write!(
                f,
                "WHERE names column '{}', which this composite does not declare — it declares: {}",
                show(column),
                List(declared)
            ),
            Self::NotLeadingPrefix { declared } => write!(
                f,
                "WHERE columns must be a leading prefix of the composite's declared order ({})",
                List(declared)
            ),
        }
    }
}

impl std::error::Error for WhereError {}
