//! Composite (multi-column) Range indexes — the ORDERPATH engine
//! piece.
//!
//! A composite index derives ONE order-preserving byte string per row
//! from several declared columns, so a single `(value, key)` B-tree
//! answers "WHERE a = x ORDER BY b DESC" the way a relational composite
//! B-tree does. The derivation is a pure mechanical byte encoding of
//! declared fields — no semantics, no planning; `IDX.VERIFY` recomputes
//! it, so drift stays falsifiable (derived-by-construction).
//!
//! ## Encoding (the exact byte rules)
//!
//! Per component, in declared order:
//! * `i64` — [`order_key`]'s sign-flipped big-endian, fixed 8 bytes.
//! * `f64` — [`order_key`]'s IEEE total-order transform, fixed 8 bytes.
//! * `str` — the raw bytes with `0x00` escaped as `0x00 0xFF`, then the
//!   terminator `0x00 0x00`. The terminator sorts below every escaped
//!   continuation byte, so a prefix string sorts first and the
//!   concatenation stays unambiguous (self-delimiting).
//! * A `DESC` component complements every byte of its framed encoding —
//!   order-reversing, and still self-delimiting because complementing
//!   is a bijection on the frame.
//!
//! Components concatenate; `memcmp` of two encodings equals the
//! column-wise tuple comparison (DESC columns reversed). A row missing
//! a component column (or one that fails coercion, or a `str` component
//! longer than [`MAX_STR_COMPONENT`]) is EXCLUDED from the composite
//! index — the same exclusion semantics a scalar coerce failure has.

use crate::catalog::ValType;
use crate::error::SpecError;
use crate::spec::IndexSpec;
use crate::value::{IndexValue, order_key};
use kevy_text::SortOrder;

#[path = "composite_where.rs"]
mod clause;
pub use clause::{WhereClause, composite_bounds, parse_where};

/// One declared composite column: which hash field, how its bytes
/// coerce/order, and whether this component sorts descending.
///
/// The type is carried per column (not looked up at read time) so the
/// sidecar reload reproduces the exact same byte derivation — an
/// encoding the catalog cannot reconstruct is index drift at boot.
///
/// ```
/// use kevy_index::{CompositeCol, SortOrder, ValType};
/// let newest_first = CompositeCol::new("at", ValType::I64).with_order(SortOrder::Desc);
/// assert_eq!(newest_first.order, SortOrder::Desc);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CompositeCol {
    /// Hash field name.
    ///
    /// ```
    /// use kevy_index::{CompositeCol, ValType, composite_encode};
    /// let col = CompositeCol::new("region", ValType::Str);
    /// assert_eq!(col.name, b"region");
    /// // the caller fetches that field from the row, in declared order
    /// assert!(composite_encode(&[col], &[Some(b"eu")]).is_some());
    /// ```
    pub name: Vec<u8>,
    /// How the column's bytes coerce (i64 | f64 | str).
    ///
    /// ```
    /// use kevy_index::{CompositeCol, ValType, composite_encode};
    /// let at = [CompositeCol::new("at", ValType::I64)];
    /// assert_eq!(at[0].ty, ValType::I64);
    /// assert!(composite_encode(&at, &[Some(b"1700000000")]).is_some());
    /// assert_eq!(composite_encode(&at, &[Some(b"yesterday")]), None, "coerce failure");
    /// ```
    pub ty: ValType,
    /// Component direction; a descending one has its bytes complemented.
    ///
    /// ```
    /// use kevy_index::{CompositeCol, SortOrder, ValType, composite_encode};
    /// let asc = [CompositeCol::new("at", ValType::I64)];
    /// let desc = [CompositeCol::new("at", ValType::I64).with_order(SortOrder::Desc)];
    /// let enc = |cols: &[CompositeCol], v: &[u8]| composite_encode(cols, &[Some(v)]);
    /// assert!(enc(&asc, b"1") < enc(&asc, b"2"));
    /// assert!(enc(&desc, b"1") > enc(&desc, b"2"), "newest first");
    /// ```
    pub order: SortOrder,
}

impl CompositeCol {
    /// An ascending column `name` of type `ty`.
    ///
    /// ```
    /// use kevy_index::{CompositeCol, SortOrder, ValType};
    /// assert_eq!(CompositeCol::new("a", ValType::Str).order, SortOrder::Asc);
    /// ```
    pub fn new(name: impl Into<Vec<u8>>, ty: ValType) -> CompositeCol {
        CompositeCol { name: name.into(), ty, order: SortOrder::Asc }
    }

    /// This column sorted in `order`.
    ///
    /// ```
    /// use kevy_index::{CompositeCol, SortOrder, ValType};
    /// let c = CompositeCol::new("a", ValType::Str).with_order(SortOrder::Desc);
    /// assert_eq!(c.order, SortOrder::Desc);
    /// ```
    #[must_use]
    pub fn with_order(mut self, order: SortOrder) -> CompositeCol {
        self.order = order;
        self
    }
}

/// Hard cap on composite columns per index.
///
/// ```
/// use kevy_index::{CompositeCol, IndexKind, IndexSpec, MAX_COMPOSITE_COLS, ValType};
/// let cols = |n: usize| (0..n).map(|i| CompositeCol::new(format!("c{i}"), ValType::I64)).collect();
/// let spec = |n| {
///     IndexSpec::builder("t.p", "t:", IndexKind::Range, ValType::Str)
///         .with_field("p")
///         .with_composite(cols(n))
///         .build()
/// };
/// assert!(spec(MAX_COMPOSITE_COLS).is_ok());
/// assert!(spec(MAX_COMPOSITE_COLS + 1).is_err());
/// ```
pub const MAX_COMPOSITE_COLS: usize = 8;

/// Hard cap on one `str` component's raw length. A longer value
/// excludes the row (documented, conformance-tested) — the same class
/// of limit a relational B-tree puts on its index row size, and what
/// keeps [`composite_bounds`]' upper bound finite and exact.
///
/// ```
/// use kevy_index::{CompositeCol, MAX_STR_COMPONENT, ValType, composite_encode};
/// let cols = [CompositeCol::new("title", ValType::Str)];
/// let at_cap = vec![b'x'; MAX_STR_COMPONENT];
/// let over = vec![b'x'; MAX_STR_COMPONENT + 1];
/// assert!(composite_encode(&cols, &[Some(&at_cap)]).is_some());
/// assert_eq!(composite_encode(&cols, &[Some(&over)]), None, "the row is excluded");
/// ```
pub const MAX_STR_COMPONENT: usize = 255;

/// The named refusal for `WHERE` on an index that declares no
/// composite columns. Shared verbatim by the server and the embedded
/// dispatch so the wire wording cannot drift.
///
/// ```
/// use kevy_index::WHERE_NOT_COMPOSITE;
/// // what a dispatcher replies when the named index has no composite columns
/// let reply = format!("-ERR {WHERE_NOT_COMPOSITE}");
/// assert!(reply.contains("requires a composite index"));
/// ```
pub const WHERE_NOT_COMPOSITE: &str =
    "WHERE requires a composite index (an ORDERPATH-compiled one) — this index is not one";

/// Encode one component. `None` = the row is excluded.
fn encode_component(col: &CompositeCol, raw: &[u8]) -> Option<Vec<u8>> {
    let mut framed = match col.ty {
        ValType::I64 | ValType::F64 => order_key(col.ty, raw)?,
        ValType::Str => {
            if raw.len() > MAX_STR_COMPONENT {
                return None;
            }
            let mut out = Vec::with_capacity(raw.len() + 2);
            for &b in raw {
                out.push(b);
                if b == 0x00 {
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0x00, 0x00]);
            out
        }
        ValType::Vector => return None,
    };
    if col.order == SortOrder::Desc {
        for b in &mut framed {
            *b = !*b;
        }
    }
    Some(framed)
}

/// The row's composite encoding: order-preserving concatenation of the
/// declared columns. `None` = the row is excluded (a missing column, a
/// coerce failure, or an over-long `str` component).
///
/// ```
/// use kevy_index::{CompositeCol, SortOrder, ValType, composite_encode};
/// let cols = [
///     CompositeCol::new("region", ValType::Str),
///     CompositeCol::new("at", ValType::I64).with_order(SortOrder::Desc),
/// ];
/// let enc = |r: &[u8], at: &[u8]| composite_encode(&cols, &[Some(r), Some(at)]);
/// // memcmp order = (region ASC, at DESC)
/// assert!(enc(b"eu", b"20") < enc(b"eu", b"10"));
/// assert!(enc(b"eu", b"10") < enc(b"us", b"99"));
/// assert_eq!(composite_encode(&cols, &[Some(b"eu"), None]), None, "missing column");
/// ```
pub fn composite_encode(cols: &[CompositeCol], vals: &[Option<&[u8]>]) -> Option<Vec<u8>> {
    composite_classify(cols, vals).into_value()
}

/// Why a row is not in an index — or the encoded value when it is.
///
/// `derive_scalar` collapsed three different exclusions into one `None`,
/// and the write path then counted every one of them as a "coerce
/// failure" — which is how a consumer's VERIFY read 30 152 coerce
/// failures that were actually rows briefly missing a column (dogfood
/// F10), and how two rows silently absent for *oversize* components
/// cost a production hunt (F8/F9). One classification now drives both
/// the write path and VERIFY, so the causes cannot drift apart.
///
/// ```
/// use kevy_index::{IndexKind, IndexSpec, RowDerivation, ValType};
/// let spec = IndexSpec::builder("age", "u:", IndexKind::Range, ValType::I64).with_field("age").build()?;
/// assert_eq!(spec.classify_scalar(&[Some(b"41".to_vec())]), RowDerivation::Indexed(b"41".to_vec()));
/// assert_eq!(spec.classify_scalar(&[None]), RowDerivation::Absent);
/// assert_eq!(spec.classify_scalar(&[Some(b"old".to_vec())]), RowDerivation::CoerceFailed);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowDerivation {
    /// The row belongs in the index, under this value.
    ///
    /// ```
    /// use kevy_index::{CompositeCol, IndexKind, IndexSpec, IndexValue, RowDerivation, ValType};
    /// let spec = IndexSpec::builder("t.p", "t:", IndexKind::Range, ValType::Str)
    ///     .with_field("p")
    ///     .with_composite(vec![CompositeCol::new("a", ValType::Str)])
    ///     .build()?;
    /// let row = [Some(b"x".to_vec())];
    /// let RowDerivation::Indexed(enc) = spec.classify_scalar(&row) else { panic!("x indexes") };
    /// // the framed bytes: raw value, then the 0x00 0x00 terminator
    /// assert_eq!(enc, b"x\0\0");
    /// assert_eq!(spec.derive_scalar(&row), Some(IndexValue::Str(enc)));
    /// // the row read fetches the composite's columns, all of them driving
    /// assert_eq!((spec.scalar_read_names(), spec.primary_width()), (vec![&b"a"[..]], 1));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Indexed(Vec<u8>),
    /// A driving column is absent from the row — NULL semantics, the
    /// row is excluded *by design* (Law 3: absence is never an error).
    ///
    /// ```
    /// use kevy_index::{CompositeCol, IndexKind, IndexSpec, RowDerivation, ValType};
    /// let spec = IndexSpec::builder("t.p", "t:", IndexKind::Range, ValType::Str)
    ///     .with_field("p")
    ///     .with_composite(vec![CompositeCol::new("a", ValType::Str), CompositeCol::new("b", ValType::I64)])
    ///     .build()?;
    /// assert_eq!(spec.classify_scalar(&[Some(b"x".to_vec()), None]), RowDerivation::Absent);
    /// assert_eq!(spec.derive_scalar(&[Some(b"x".to_vec()), None]), None);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Absent,
    /// A present value failed to coerce to the declared type.
    ///
    /// ```
    /// use kevy_index::{IndexKind, IndexSpec, RowDerivation, ValType};
    /// let spec = IndexSpec::builder("px", "p:", IndexKind::Range, ValType::F64).with_field("px").build()?;
    /// assert_eq!(spec.classify_scalar(&[Some(b"NaN".to_vec())]), RowDerivation::CoerceFailed);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    CoerceFailed,
    /// A string component exceeded [`MAX_STR_COMPONENT`]; the row is
    /// excluded from this composite (documented bound — hash or bound
    /// the column if every row must index).
    ///
    /// ```
    /// use kevy_index::{CompositeCol, IndexKind, IndexSpec, MAX_STR_COMPONENT, RowDerivation, ValType};
    /// let spec = IndexSpec::builder("t.p", "t:", IndexKind::Range, ValType::Str)
    ///     .with_field("p")
    ///     .with_composite(vec![CompositeCol::new("title", ValType::Str)])
    ///     .build()?;
    /// let long = vec![b'x'; MAX_STR_COMPONENT + 1];
    /// assert_eq!(spec.classify_scalar(&[Some(long)]), RowDerivation::Oversize);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Oversize,
}

impl RowDerivation {
    fn into_value(self) -> Option<Vec<u8>> {
        match self {
            Self::Indexed(v) => Some(v),
            _ => None,
        }
    }
}

/// [`composite_encode`] with the exclusion cause kept. First failing
/// component decides, in column order — deterministic, so VERIFY's
/// tallies are stable.
pub fn composite_classify(cols: &[CompositeCol], vals: &[Option<&[u8]>]) -> RowDerivation {
    let mut out = Vec::new();
    for (col, raw) in cols.iter().zip(vals) {
        let Some(raw) = raw else { return RowDerivation::Absent };
        match classify_component(col, raw) {
            RowDerivation::Indexed(bytes) => out.extend_from_slice(&bytes),
            other => return other,
        }
    }
    RowDerivation::Indexed(out)
}

/// [`encode_component`]'s cause-aware form; the encoder delegates here
/// so the two can never disagree.
fn classify_component(col: &CompositeCol, raw: &[u8]) -> RowDerivation {
    if col.ty == ValType::Str && raw.len() > MAX_STR_COMPONENT {
        return RowDerivation::Oversize;
    }
    match encode_component(col, raw) {
        Some(bytes) => RowDerivation::Indexed(bytes),
        // Str oversize was handled above; what remains is a numeric
        // component that did not parse (or a Vector column, which never
        // composites) — a coercion failure either way.
        None => RowDerivation::CoerceFailed,
    }
}

/// The CREATE-time guard: composite is legal ONLY on `KIND range` with
/// `TYPE str` (the derived value IS a byte string), a single declared
/// FIELD, no stored VALUES, and 1..=[`MAX_COMPOSITE_COLS`] columns of
/// scalar types. Every refused combo errors by name.
pub(crate) fn composite_guard(spec: &IndexSpec) -> Result<(), SpecError> {
    let Some(cols) = &spec.composite else { return Ok(()) };
    if spec.kind != crate::IndexKind::Range {
        return Err(SpecError::CompositeNeedsRange);
    }
    if spec.ty != ValType::Str {
        return Err(SpecError::CompositeNeedsStr);
    }
    if !spec.values.is_empty() {
        return Err(SpecError::CompositeWithValues);
    }
    if spec.fields.len() != 1 {
        return Err(SpecError::CompositeFieldCount);
    }
    if cols.is_empty() {
        return Err(SpecError::CompositeNoColumns);
    }
    if cols.len() > MAX_COMPOSITE_COLS {
        return Err(SpecError::CompositeTooManyColumns);
    }
    if cols.iter().any(|c| matches!(c.ty, ValType::Vector)) {
        return Err(SpecError::CompositeColumnType);
    }
    Ok(())
}

impl IndexSpec {
    /// Column names a scalar (range/unique) row read fetches, in
    /// order: the driving columns — the composite's declared columns,
    /// or the single `FIELD` — then the declared `VALUES` columns.
    /// One row peek covers everything (the one-pread-per-row rule).
    pub fn scalar_read_names(&self) -> Vec<&[u8]> {
        let mut names: Vec<&[u8]> = match &self.composite {
            Some(cols) => cols.iter().map(|c| c.name.as_slice()).collect(),
            None => vec![self.field()],
        };
        names.extend(self.values.iter().map(|v| v.name.as_slice()));
        names
    }

    /// How many leading [`Self::scalar_read_names`] drive the index
    /// value (the rest are stored `VALUES`).
    pub fn primary_width(&self) -> usize {
        self.composite.as_ref().map_or(1, Vec::len)
    }

    /// Derive the index value from the fetched driving columns
    /// (parallel to the first [`Self::primary_width`] names). `None` =
    /// the row is excluded. This is THE single derivation both the
    /// server and the embedded store apply — and what `IDX.VERIFY`
    /// recomputes, so composite drift is falsifiable too.
    pub fn derive_scalar(&self, prim: &[Option<Vec<u8>>]) -> Option<IndexValue> {
        match &self.composite {
            // Composites carry their order-preserving byte encoding.
            Some(_) => match self.classify_scalar(prim) {
                RowDerivation::Indexed(v) => Some(IndexValue::Str(v)),
                _ => None,
            },
            // Singles carry the typed value — the classification's raw
            // bytes re-coerce through the same function it used, so the
            // two answers cannot part.
            None => IndexValue::coerce(self.ty, prim.first()?.as_deref()?),
        }
    }

    /// [`Self::derive_scalar`] with the exclusion cause kept — VERIFY's
    /// row→index direction reads this, the write path reads
    /// `derive_scalar`, and both stand on the same classification.
    pub fn classify_scalar(&self, prim: &[Option<Vec<u8>>]) -> RowDerivation {
        match &self.composite {
            Some(cols) => {
                let refs: Vec<Option<&[u8]>> = prim.iter().map(|o| o.as_deref()).collect();
                composite_classify(cols, &refs)
            }
            None => match prim.first().and_then(|o| o.as_deref()) {
                None => RowDerivation::Absent,
                Some(raw) => match IndexValue::coerce(self.ty, raw) {
                    Some(_) => RowDerivation::Indexed(raw.to_vec()),
                    None => RowDerivation::CoerceFailed,
                },
            },
        }
    }
}

#[cfg(test)]
#[path = "composite_tests.rs"]
mod tests;
