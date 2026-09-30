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
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::I64);
    /// assert_eq!(b.build().err(), Some(SpecError::NoFields));
    /// ```
    NoFields,
    /// Several fields on a kind other than text.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Unique, T::Str).with_field("a").with_field("b");
    /// assert_eq!(b.build().err(), Some(SpecError::SeveralFieldsNotText));
    /// ```
    SeveralFieldsNotText,
    /// `WITH POSITIONS` on a kind other than text.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::Str).with_field("f").with_positions(true);
    /// assert_eq!(b.build().err(), Some(SpecError::PositionsNotText));
    /// ```
    PositionsNotText,
    /// `VALUES` on a kind with no stored-value column.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Agg, T::I64).with_field("f").with_values(vec![kevy_index::ValueSpec::new("v")]);
    /// assert_eq!(b.build().err(), Some(SpecError::ValuesKind));
    /// ```
    ValuesKind,
    /// `KIND ann` without `TYPE vector` and `DIM`.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Ann, T::Vector).with_field("f");
    /// assert_eq!(b.build().err(), Some(SpecError::AnnNeedsVector));
    /// ```
    AnnNeedsVector,
    /// `TYPE vector` on a kind other than ann.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::Vector).with_field("f");
    /// assert_eq!(b.build().err(), Some(SpecError::VectorNeedsAnn));
    /// ```
    VectorNeedsAnn,
    /// ANN parameters on a kind other than ann.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::Str).with_field("f").with_ann(AnnSpec::new(3));
    /// assert_eq!(b.build().err(), Some(SpecError::AnnParamsNeedAnn));
    /// ```
    AnnParamsNeedAnn,
    /// `KIND agg` without `GROUPBY`.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Agg, T::I64).with_field("f");
    /// assert_eq!(b.build().err(), Some(SpecError::AggNeedsGroupBy));
    /// ```
    AggNeedsGroupBy,
    /// `KIND agg` on a type other than i64 or f64.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Agg, T::Str).with_field("f").with_group_by("g");
    /// assert_eq!(b.build().err(), Some(SpecError::AggNeedsNumber));
    /// ```
    AggNeedsNumber,
    /// `GROUPBY` on a kind other than agg.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::Str).with_field("f").with_group_by("g");
    /// assert_eq!(b.build().err(), Some(SpecError::GroupByNeedsAgg));
    /// ```
    GroupByNeedsAgg,
    /// `COMPOSITE` on a kind other than range.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let cols = vec![CompositeCol::new("a", T::I64)];
    /// let b = IndexSpec::builder("n", "p:", K::Unique, T::Str).with_field("f").with_composite(cols);
    /// assert_eq!(b.build().err(), Some(SpecError::CompositeNeedsRange));
    /// ```
    CompositeNeedsRange,
    /// `COMPOSITE` on a type other than str.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let cols = vec![CompositeCol::new("a", T::I64)];
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::I64).with_field("f").with_composite(cols);
    /// assert_eq!(b.build().err(), Some(SpecError::CompositeNeedsStr));
    /// ```
    CompositeNeedsStr,
    /// `COMPOSITE` together with `VALUES`.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let cols = vec![CompositeCol::new("a", T::I64)];
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::Str).with_field("f").with_composite(cols);
    /// let b = b.with_values(vec![kevy_index::ValueSpec::new("v")]);
    /// assert_eq!(b.build().err(), Some(SpecError::CompositeWithValues));
    /// ```
    CompositeWithValues,
    /// `COMPOSITE` with other than one `FIELD`.
    ///
    /// The field-count checks run first ([`SpecError::NoFields`],
    /// [`SpecError::SeveralFieldsNotText`]), so the builder reports those
    /// instead; this variant names the composite rule on its own.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let cols = vec![CompositeCol::new("a", T::I64)];
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::Str).with_fields(Vec::new());
    /// // no field at all is refused before the composite is looked at
    /// assert_eq!(b.with_composite(cols).build().err(), Some(SpecError::NoFields));
    /// assert!(SpecError::CompositeFieldCount.as_wire().contains("exactly one FIELD"));
    /// ```
    CompositeFieldCount,
    /// `COMPOSITE` with no columns.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::Str).with_field("f").with_composite(Vec::new());
    /// assert_eq!(b.build().err(), Some(SpecError::CompositeNoColumns));
    /// ```
    CompositeNoColumns,
    /// `COMPOSITE` with more than [`crate::MAX_COMPOSITE_COLS`] columns.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let cols = (0..9).map(|i| CompositeCol::new(format!("c{i}"), T::I64)).collect();
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::Str).with_field("f").with_composite(cols);
    /// assert_eq!(b.build().err(), Some(SpecError::CompositeTooManyColumns));
    /// ```
    CompositeTooManyColumns,
    /// A `COMPOSITE` column of a type other than i64, f64 or str.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, CompositeCol, IndexKind as K, IndexSpec, SpecError, ValType as T};
    /// let b = IndexSpec::builder("n", "p:", K::Range, T::Str).with_field("f").with_composite(vec![CompositeCol::new("v", T::Vector)]);
    /// assert_eq!(b.build().err(), Some(SpecError::CompositeColumnType));
    /// ```
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
    ///
    /// ```
    /// use kevy_index::{CatalogError, IndexValue, Leaf, Tree, ViewCatalog, ViewError, ViewSpec};
    /// let leaf = || Box::new(Tree::Leaf(Leaf::new("n", IndexValue::I64(0), IndexValue::I64(1))));
    /// let deep = Tree::Or(Box::new(Tree::Or(Box::new(Tree::Or(leaf(), leaf())), leaf())), leaf());
    /// let refused = ViewCatalog::new().create(ViewSpec::new("v", deep, "n"));
    /// assert_eq!(refused, Err(CatalogError::View(ViewError::TooDeep)));
    /// ```
    TooDeep,
    /// The tree has more than [`crate::MAX_TREE_LEAVES`] leaves.
    ///
    /// Trees are binary, so a tree over the leaf cap is also over the
    /// depth cap, and `validate` reports the depth first; the variant
    /// keeps the leaf cap named on its own.
    ///
    /// ```
    /// use kevy_index::{CatalogError, ViewError};
    /// let e = ViewError::TooManyLeaves;
    /// assert_eq!(e.to_string(), "view tree has more than 4 leaves");
    /// assert_eq!(CatalogError::from(e).to_wire(), e.as_wire());
    /// ```
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
    ///
    /// ```
    /// use kevy_index::{Catalog, CatalogError, Declared, IndexKind, IndexSpec, ValType};
    /// let spec = || IndexSpec::builder("age", "u:", IndexKind::Range, ValType::I64).with_field("age").build();
    /// let mut c = Catalog::new();
    /// c.create(spec()?)?;
    /// assert_eq!(c.create(spec()?), Err(CatalogError::Exists(Declared::Index)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Index,
    /// A view ([`crate::ViewCatalog`]).
    ///
    /// ```
    /// use kevy_index::{CatalogError, Declared, IndexValue, Leaf, Tree, ViewCatalog, ViewSpec};
    /// let spec = || ViewSpec::new("v", Tree::Leaf(Leaf::new("n", IndexValue::I64(0), IndexValue::I64(1))), "n");
    /// let mut views = ViewCatalog::new();
    /// views.create(spec())?;
    /// assert_eq!(views.create(spec()), Err(CatalogError::Exists(Declared::View)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    View,
    /// A table ([`crate::TableCatalog`]).
    ///
    /// ```
    /// use kevy_index::{CatalogError, Declared, TableCatalog, parse_table_declare};
    /// let argv: [&[u8]; 9] = [b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64"];
    /// let mut tables = TableCatalog::new();
    /// tables.create(parse_table_declare(&argv)?)?;
    /// let again = tables.create(parse_table_declare(&argv)?);
    /// assert_eq!(again.map_err(|e| e.to_wire()), Err("ERR table already exists".to_string()));
    /// assert!(matches!(tables.create(parse_table_declare(&argv)?), Err(CatalogError::Exists(Declared::Table))));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
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
    ///
    /// ```
    /// use kevy_index::{Catalog, CatalogError, Declared, IndexKind, IndexSpec, ValType};
    /// let mut c = Catalog::new();
    /// let refused = (0..).find_map(|i| {
    ///     let spec = IndexSpec::builder(format!("i{i}"), "p:", IndexKind::Range, ValType::I64);
    ///     c.create(spec.with_field("n").build().ok()?).err()
    /// });
    /// assert_eq!(refused, Some(CatalogError::Full(Declared::Index)));
    /// assert_eq!(c.len(), 64);
    /// ```
    Full(Declared),
    /// A declaration of that name exists.
    ///
    /// ```
    /// use kevy_index::{Catalog, CatalogError, Declared, IndexKind, IndexSpec, ValType};
    /// let mut c = Catalog::new();
    /// let spec = |prefix: &str| IndexSpec::builder("age", prefix, IndexKind::Range, ValType::I64).with_field("age").build();
    /// c.create(spec("user:")?)?;
    /// // names are unique even when the domains differ
    /// assert_eq!(c.create(spec("admin:")?), Err(CatalogError::Exists(Declared::Index)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Exists(Declared),
    /// `PARTITION global` on an index kind whose segments are not a
    /// `(value, key)` order.
    ///
    /// ```
    /// use kevy_index::{Catalog, CatalogError, IndexKind, IndexSpec, Partitioning, ValType};
    /// let text = IndexSpec::builder("body", "doc:", IndexKind::Text, ValType::Str).with_field("body").build()?;
    /// let refused = Catalog::new().create_with(text, Partitioning::Global { splits: Vec::new() });
    /// assert_eq!(refused, Err(CatalogError::GlobalNeedsOrder));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    GlobalNeedsOrder,
    /// `SPLIT AT` values that are not strictly increasing.
    ///
    /// ```
    /// use kevy_index::{Catalog, CatalogError, IndexKind, IndexSpec, Partitioning, ValType};
    /// let age = IndexSpec::builder("age", "u:", IndexKind::Range, ValType::I64).with_field("age").build()?;
    /// let splits = vec![b"m".to_vec(), b"c".to_vec()];
    /// let refused = Catalog::new().create_with(age, Partitioning::Global { splits });
    /// assert_eq!(refused, Err(CatalogError::SplitsOutOfOrder));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    SplitsOutOfOrder,
    /// The view failed its structural caps.
    ///
    /// ```
    /// use kevy_index::{CatalogError, ViewError};
    /// let e = CatalogError::from(ViewError::TooDeep);
    /// assert_eq!(e, CatalogError::View(ViewError::TooDeep));
    /// assert!(std::error::Error::source(&e).is_some(), "the view error is the cause");
    /// ```
    View(ViewError),
    /// The table failed its validation.
    ///
    /// ```
    /// use kevy_index::{CatalogError, TableCatalog, TableError, parse_table_declare};
    /// let mut t = parse_table_declare(&[b"TABLE.DECLARE", b"t", b"PREFIX", b"t:", b"PK", b"id", b"COLUMN", b"id", b"i64"])?;
    /// t.prefix.clear();
    /// // admission re-validates, so a spec edited after parsing is still checked
    /// assert_eq!(TableCatalog::new().create(t), Err(CatalogError::Table(TableError::EmptyPrefix)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
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

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
