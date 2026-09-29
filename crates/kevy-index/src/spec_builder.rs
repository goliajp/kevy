//! [`IndexSpecBuilder`] and the agreement rules its `build` enforces.

use crate::catalog::{IndexKind, ValType};
use crate::composite::CompositeCol;
use crate::spec::IndexSpec;
use crate::spec_parts::{AnnSpec, FieldSpec, ValueSpec};

/// Assembles an [`IndexSpec`]; from [`IndexSpec::builder`].
///
/// ```
/// use kevy_index::{IndexKind, IndexSpec, ValType};
/// let b = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64);
/// assert_eq!(b.clone().build().err(), Some("ERR index needs at least one field"));
/// assert!(b.with_field("f").build().is_ok());
/// ```
#[derive(Debug, Clone)]
#[must_use]
pub struct IndexSpecBuilder {
    pub(crate) spec: IndexSpec,
}

impl IndexSpecBuilder {
    /// Append a neutrally-weighted field.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("t", "d:", IndexKind::Text, ValType::Str)
    ///     .with_field("title")
    ///     .with_field("body")
    ///     .build()?;
    /// assert_eq!(s.fields().len(), 2);
    /// # Ok::<(), &'static str>(())
    /// ```
    pub fn with_field(mut self, name: impl Into<Vec<u8>>) -> Self {
        self.spec.fields.push(FieldSpec::new(name));
        self
    }

    /// Replace the field list.
    ///
    /// ```
    /// # use kevy_index::{FieldSpec, IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("t", "d:", IndexKind::Text, ValType::Str)
    ///     .with_fields(vec![FieldSpec::new("title").with_weight(2.0)])
    ///     .build()?;
    /// assert_eq!(s.fields()[0].weight, 2.0);
    /// # Ok::<(), &'static str>(())
    /// ```
    pub fn with_fields(mut self, fields: Vec<FieldSpec>) -> Self {
        self.spec.fields = fields;
        self
    }

    /// Set the byte budget (`MAXMEM`); 0 = unlimited.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let b = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64).with_field("f");
    /// assert_eq!(b.with_max_bytes(1).build()?.max_bytes(), 1);
    /// # Ok::<(), &'static str>(())
    /// ```
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.spec.max_bytes = max_bytes;
        self
    }

    /// Set the HNSW parameters (`KIND ann` only).
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, IndexKind, IndexSpec, ValType};
    /// let b = IndexSpec::builder("v", "p:", IndexKind::Ann, ValType::Vector).with_field("e");
    /// assert_eq!(b.clone().build().err(), Some("ERR KIND ann requires TYPE vector and DIM"));
    /// assert!(b.with_ann(AnnSpec::new(2)).build().is_ok());
    /// ```
    pub fn with_ann(mut self, ann: AnnSpec) -> Self {
        self.spec.ann = Some(ann);
        self
    }

    /// Set the grouping field (`KIND agg` only).
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let b = IndexSpec::builder("g", "o:", IndexKind::Agg, ValType::I64).with_field("amount");
    /// assert_eq!(b.clone().build().err(), Some("ERR KIND agg requires GROUPBY <field>"));
    /// assert!(b.with_group_by("status").build().is_ok());
    /// ```
    pub fn with_group_by(mut self, field: impl Into<Vec<u8>>) -> Self {
        self.spec.group_by = Some(field.into());
        self
    }

    /// Record token positions (`WITH POSITIONS`, `KIND text` only).
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let b = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64).with_field("f");
    /// assert_eq!(b.with_positions(true).build().err(), Some("ERR WITH POSITIONS requires KIND text"));
    /// ```
    pub fn with_positions(mut self, on: bool) -> Self {
        self.spec.with_positions = on;
        self
    }

    /// Set the stored `VALUES` fields (`KIND text|range|unique`).
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType, ValueSpec};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64)
    ///     .with_field("f")
    ///     .with_values(vec![ValueSpec::new("tag")])
    ///     .build()?;
    /// assert_eq!(s.values().len(), 1);
    /// # Ok::<(), &'static str>(())
    /// ```
    pub fn with_values(mut self, values: Vec<ValueSpec>) -> Self {
        self.spec.values = values;
        self
    }

    /// Set the composite columns (a `range` index over `str`, one field,
    /// no `VALUES`).
    ///
    /// ```
    /// # use kevy_index::{CompositeCol, IndexKind, IndexSpec, ValType};
    /// let b = IndexSpec::builder("t.p", "t:", IndexKind::Unique, ValType::Str)
    ///     .with_field("a")
    ///     .with_composite(vec![CompositeCol::new("a", ValType::Str)]);
    /// assert_eq!(b.build().err(), Some("ERR COMPOSITE requires KIND range"));
    /// ```
    pub fn with_composite(mut self, cols: Vec<CompositeCol>) -> Self {
        self.spec.composite = Some(cols);
        self
    }

    /// The spec, or the first way its parts disagree, named the way the
    /// wire refuses it.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let two = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64)
    ///     .with_field("a")
    ///     .with_field("b")
    ///     .build();
    /// assert_eq!(two.err(), Some("ERR only KIND text indexes several fields"));
    /// ```
    pub fn build(self) -> Result<IndexSpec, &'static str> {
        let s = self.spec;
        fields_guard(&s)?;
        kind_guard(&s)?;
        crate::composite::composite_guard(&s)?;
        Ok(s)
    }
}

/// The field list against the kind: at least one, several only on text,
/// positions only on text, `VALUES` only where a stored column exists.
fn fields_guard(s: &IndexSpec) -> Result<(), &'static str> {
    if s.fields.is_empty() {
        return Err("ERR index needs at least one field");
    }
    // every kind but text reads one scalar: a second field would be
    // declared and never consulted
    if s.fields.len() > 1 && s.kind != IndexKind::Text {
        return Err("ERR only KIND text indexes several fields");
    }
    // only the text segment maintains the positional side-channel
    if s.with_positions && s.kind != IndexKind::Text {
        return Err("ERR WITH POSITIONS requires KIND text");
    }
    // ann and agg carry no stored-value column to fill or filter on
    if !s.values.is_empty()
        && !matches!(s.kind, IndexKind::Text | IndexKind::Range | IndexKind::Unique)
    {
        return Err("ERR VALUES requires KIND text|range|unique");
    }
    Ok(())
}

/// The kind-specific parts: ANN parameters and a vector type exactly on
/// `ann`, a grouping field and a numeric type exactly on `agg`.
fn kind_guard(s: &IndexSpec) -> Result<(), &'static str> {
    let ann = s.kind == IndexKind::Ann;
    if ann && (s.ann.is_none() || s.ty != ValType::Vector) {
        return Err("ERR KIND ann requires TYPE vector and DIM");
    }
    if !ann && s.ty == ValType::Vector {
        return Err("ERR TYPE vector requires KIND ann");
    }
    if !ann && s.ann.is_some() {
        return Err("ERR ANN parameters require KIND ann");
    }
    let agg = s.kind == IndexKind::Agg;
    if agg && s.group_by.is_none() {
        return Err("ERR KIND agg requires GROUPBY <field>");
    }
    if agg && !matches!(s.ty, ValType::I64 | ValType::F64) {
        return Err("ERR KIND agg requires TYPE i64|f64");
    }
    if !agg && s.group_by.is_some() {
        return Err("ERR GROUPBY requires KIND agg");
    }
    Ok(())
}
