//! Why an index, view or catalog admission was refused. Each error words
//! itself the way the wire refuses it: `as_wire` / `to_wire` is the reply
//! text with its `ERR` code, `Display` the same text without the code.

use std::fmt;

use crate::table_error::TableError;

/// The text after a wire error's code: `ERR x` reads as `x`.
fn without_code(wire: &str) -> &str {
    wire.split_once(' ').map_or(wire, |(_, text)| text)
}

/// Why [`crate::IndexSpecBuilder::build`] refused: the first way the
/// declaration's parts disagree.
///
/// ```
/// use kevy_index::{IndexKind, IndexSpec, SpecError, ValType};
///
/// let e = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64).build().unwrap_err();
/// assert_eq!(e, SpecError::NoFields);
/// assert_eq!(e.as_wire(), "ERR index needs at least one field");
/// assert_eq!(e.to_string(), "index needs at least one field");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SpecError {
    /// No `FIELD` declared.
    NoFields,
    /// Several fields on a kind other than text.
    SeveralFieldsNotText,
    /// `WITH POSITIONS` on a kind other than text.
    PositionsNotText,
    /// `VALUES` on a kind with no stored-value column.
    ValuesKind,
    /// `KIND ann` without `TYPE vector` and `DIM`.
    AnnNeedsVector,
    /// `TYPE vector` on a kind other than ann.
    VectorNeedsAnn,
    /// ANN parameters on a kind other than ann.
    AnnParamsNeedAnn,
    /// `KIND agg` without `GROUPBY`.
    AggNeedsGroupBy,
    /// `KIND agg` on a type other than i64 or f64.
    AggNeedsNumber,
    /// `GROUPBY` on a kind other than agg.
    GroupByNeedsAgg,
    /// `COMPOSITE` on a kind other than range.
    CompositeNeedsRange,
    /// `COMPOSITE` on a type other than str.
    CompositeNeedsStr,
    /// `COMPOSITE` together with `VALUES`.
    CompositeWithValues,
    /// `COMPOSITE` with other than one `FIELD`.
    CompositeFieldCount,
    /// `COMPOSITE` with no columns.
    CompositeNoColumns,
    /// `COMPOSITE` with more than [`crate::MAX_COMPOSITE_COLS`] columns.
    CompositeTooManyColumns,
    /// A `COMPOSITE` column of a type other than i64, f64 or str.
    CompositeColumnType,
}

impl SpecError {
    /// The refusal as the wire words it.
    ///
    /// ```
    /// assert_eq!(kevy_index::SpecError::VectorNeedsAnn.as_wire(), "ERR TYPE vector requires KIND ann");
    /// ```
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::NoFields => "ERR index needs at least one field",
            Self::SeveralFieldsNotText => "ERR only KIND text indexes several fields",
            Self::PositionsNotText => "ERR WITH POSITIONS requires KIND text",
            Self::ValuesKind => "ERR VALUES requires KIND text|range|unique",
            Self::AnnNeedsVector => "ERR KIND ann requires TYPE vector and DIM",
            Self::VectorNeedsAnn => "ERR TYPE vector requires KIND ann",
            Self::AnnParamsNeedAnn => "ERR ANN parameters require KIND ann",
            Self::AggNeedsGroupBy => "ERR KIND agg requires GROUPBY <field>",
            Self::AggNeedsNumber => "ERR KIND agg requires TYPE i64|f64",
            Self::GroupByNeedsAgg => "ERR GROUPBY requires KIND agg",
            Self::CompositeNeedsRange => "ERR COMPOSITE requires KIND range",
            Self::CompositeNeedsStr => "ERR COMPOSITE requires TYPE str",
            Self::CompositeWithValues => "ERR COMPOSITE cannot combine with VALUES",
            Self::CompositeFieldCount => "ERR COMPOSITE declares exactly one FIELD",
            Self::CompositeNoColumns => "ERR COMPOSITE needs at least one column",
            Self::CompositeTooManyColumns => "ERR COMPOSITE supports at most 8 columns",
            Self::CompositeColumnType => "ERR COMPOSITE columns must be i64|f64|str",
        }
    }
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(without_code(self.as_wire()))
    }
}

impl std::error::Error for SpecError {}

/// Why [`crate::ViewSpec::validate`] refused: a structural cap.
///
/// ```
/// assert_eq!(kevy_index::ViewError::TooDeep.as_wire(), "ERR view tree deeper than 3");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ViewError {
    /// The tree is deeper than [`crate::MAX_TREE_DEPTH`].
    TooDeep,
    /// The tree has more than [`crate::MAX_TREE_LEAVES`] leaves.
    TooManyLeaves,
}

impl ViewError {
    /// The refusal as the wire words it.
    ///
    /// ```
    /// let e = kevy_index::ViewError::TooManyLeaves;
    /// assert_eq!(e.as_wire(), "ERR view tree has more than 4 leaves");
    /// ```
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::TooDeep => "ERR view tree deeper than 3",
            Self::TooManyLeaves => "ERR view tree has more than 4 leaves",
        }
    }
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(without_code(self.as_wire()))
    }
}

impl std::error::Error for ViewError {}

/// What a catalog holds: the subject of a [`CatalogError`].
///
/// ```
/// assert_eq!(kevy_index::Declared::View.as_str(), "view");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Declared {
    /// An index ([`crate::Catalog`]).
    Index,
    /// A view ([`crate::ViewCatalog`]).
    View,
    /// A table ([`crate::TableCatalog`]).
    Table,
}

impl Declared {
    /// The noun the wire uses for it.
    ///
    /// ```
    /// assert_eq!(kevy_index::Declared::Table.as_str(), "table");
    /// ```
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::View => "view",
            Self::Table => "table",
        }
    }

    fn cap(self) -> usize {
        match self {
            Self::Index => crate::catalog::MAX_INDEXES,
            Self::View => crate::view_sidecar::MAX_VIEWS,
            Self::Table => crate::table::MAX_TABLES,
        }
    }
}

/// Why a catalog refused to admit a declaration.
///
/// ```
/// use kevy_index::{CatalogError, Declared};
///
/// let e = CatalogError::Exists(Declared::Index);
/// assert_eq!(e.to_wire(), "ERR index already exists");
/// assert_eq!(e.to_string(), "index already exists");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CatalogError {
    /// The catalog already holds its cap of declarations.
    Full(Declared),
    /// A declaration of that name exists.
    Exists(Declared),
    /// `PARTITION global` on an index kind whose segments are not a
    /// `(value, key)` order.
    GlobalNeedsOrder,
    /// `SPLIT AT` values that are not strictly increasing.
    SplitsOutOfOrder,
    /// The view failed its structural caps.
    View(ViewError),
    /// The table failed its validation.
    Table(TableError),
}

impl CatalogError {
    /// The refusal as the wire words it.
    ///
    /// ```
    /// let e = kevy_index::CatalogError::Full(kevy_index::Declared::Table);
    /// assert_eq!(e.to_wire(), "ERR table limit reached (64)");
    /// ```
    pub fn to_wire(&self) -> String {
        format!("ERR {self}")
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(d) => write!(f, "{} limit reached ({})", d.as_str(), d.cap()),
            Self::Exists(d) => write!(f, "{} already exists", d.as_str()),
            Self::GlobalNeedsOrder => f.write_str("PARTITION global requires KIND range|unique"),
            Self::SplitsOutOfOrder => f.write_str("SPLIT AT values must be strictly increasing"),
            Self::View(e) => write!(f, "{e}"),
            Self::Table(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(e) => Some(e),
            Self::Table(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ViewError> for CatalogError {
    fn from(e: ViewError) -> Self {
        Self::View(e)
    }
}

impl From<TableError> for CatalogError {
    fn from(e: TableError) -> Self {
        Self::Table(e)
    }
}
