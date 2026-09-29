//! [`IndexSpec`] — one index declaration, and the builder that is the
//! only way to make one, so every spec in existence already agrees
//! with itself.

use crate::catalog::{IndexKind, ValType};
use crate::composite::CompositeCol;
use crate::spec_builder::IndexSpecBuilder;
use crate::spec_parts::{AnnSpec, FieldSpec, ValueSpec};

/// One declared index.
///
/// The parts of a declaration constrain each other — ANN parameters
/// exist exactly on `KIND ann`, a grouping field exactly on `KIND agg`,
/// several fields and positions only on `KIND text`, composite columns
/// only on a `range` index over `str` — so the fields are private and the
/// only way to a spec is [`IndexSpec::builder`], whose
/// [`build`](IndexSpecBuilder::build) refuses a combination that
/// disagrees. Every spec a catalog, a segment or a sidecar sees is
/// therefore already consistent; readers use the accessors.
///
/// ```
/// use kevy_index::{IndexKind, IndexSpec, ValType};
///
/// let age = IndexSpec::builder("age", "user:", IndexKind::Range, ValType::I64)
///     .with_field("age")
///     .build()?;
/// assert_eq!((age.name(), age.field(), age.kind()), (&b"age"[..], &b"age"[..], IndexKind::Range));
///
/// // a grouping field on a range index is refused, not ignored
/// let bad = IndexSpec::builder("x", "user:", IndexKind::Range, ValType::I64)
///     .with_field("age")
///     .with_group_by("city")
///     .build();
/// assert_eq!(bad.err().map(|e| e.as_wire()), Some("ERR GROUPBY requires KIND agg"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSpec {
    pub(crate) name: Vec<u8>,
    pub(crate) prefix: Vec<u8>,
    // no single-field twin is kept beside this: two sources of truth for
    // "which field" is the shape that drifts
    pub(crate) fields: Vec<FieldSpec>,
    pub(crate) ty: ValType,
    pub(crate) kind: IndexKind,
    pub(crate) max_bytes: u64,
    pub(crate) ann: Option<AnnSpec>,
    pub(crate) group_by: Option<Vec<u8>>,
    pub(crate) with_positions: bool,
    pub(crate) values: Vec<ValueSpec>,
    pub(crate) composite: Option<Vec<CompositeCol>>,
}

/// What one row looks like to an index: each declared field's raw bytes
/// with its BM25 weight, and each declared `VALUES` field's raw bytes
/// (`None` where the row has none).
pub type RowInputs = (Vec<(Vec<u8>, f32)>, Vec<Option<Vec<u8>>>);

impl IndexSpec {
    /// Start declaring index `name` over keys under `prefix`. Add at
    /// least one field, then [`build`](IndexSpecBuilder::build).
    ///
    /// ```
    /// use kevy_index::{FieldSpec, IndexKind, IndexSpec, ValType};
    /// let docs = IndexSpec::builder("docs", "doc:", IndexKind::Text, ValType::Str)
    ///     .with_fields(vec![FieldSpec::new("title").with_weight(2.0), FieldSpec::new("body")])
    ///     .with_positions(true)
    ///     .build()?;
    /// assert_eq!(docs.fields().len(), 2);
    /// assert!(docs.has_positions());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn builder(
        name: impl Into<Vec<u8>>,
        prefix: impl Into<Vec<u8>>,
        kind: IndexKind,
        ty: ValType,
    ) -> IndexSpecBuilder {
        IndexSpecBuilder {
            spec: IndexSpec {
                name: name.into(),
                prefix: prefix.into(),
                fields: Vec::new(),
                ty,
                kind,
                max_bytes: 0,
                ann: None,
                group_by: None,
                with_positions: false,
                values: Vec::new(),
                composite: None,
            },
        }
    }

    /// Unique catalog name.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64).with_field("f").build()?;
    /// assert_eq!(s.name(), b"n");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    /// Key-prefix domain (`ON PREFIX user:`).
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64).with_field("f").build()?;
    /// assert_eq!(s.prefix(), b"p:");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn prefix(&self) -> &[u8] {
        &self.prefix
    }

    /// Hash fields the value comes from, in declaration order; never
    /// empty. Single-field indexes are the one-element case.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64).with_field("f").build()?;
    /// assert_eq!(s.fields()[0].name, b"f");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn fields(&self) -> &[FieldSpec] {
        &self.fields
    }

    /// The primary field — the first declared one. Every kind except
    /// text indexes exactly one attribute.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64).with_field("f").build()?;
    /// assert_eq!(s.field(), b"f");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn field(&self) -> &[u8] {
        self.fields.first().map_or(&[][..], |f| f.name.as_slice())
    }

    /// Declared scalar type.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::F64).with_field("f").build()?;
    /// assert_eq!(s.ty(), ValType::F64);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ty(&self) -> ValType {
        self.ty
    }

    /// The index kind.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Unique, ValType::Str).with_field("f").build()?;
    /// assert_eq!(s.kind(), IndexKind::Unique);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn kind(&self) -> IndexKind {
        self.kind
    }

    /// Per-index byte budget (`MAXMEM`); 0 = unlimited.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64)
    ///     .with_field("f")
    ///     .with_max_bytes(4096)
    ///     .build()?;
    /// assert_eq!(s.max_bytes(), 4096);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// ANN parameters: `Some` exactly on `KIND ann`.
    ///
    /// ```
    /// use kevy_index::{AnnSpec, IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("v", "p:", IndexKind::Ann, ValType::Vector)
    ///     .with_field("emb")
    ///     .with_ann(AnnSpec::new(3))
    ///     .build()?;
    /// assert_eq!(s.ann().map(|a| a.dim), Some(3));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn ann(&self) -> Option<AnnSpec> {
        self.ann
    }

    /// Grouping field: `Some` exactly on `KIND agg`.
    ///
    /// ```
    /// use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("g", "o:", IndexKind::Agg, ValType::I64)
    ///     .with_field("amount")
    ///     .with_group_by("status")
    ///     .build()?;
    /// assert_eq!(s.group_by(), Some(&b"status"[..]));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn group_by(&self) -> Option<&[u8]> {
        self.group_by.as_deref()
    }

    /// Whether token positions are recorded (`WITH POSITIONS`, text only),
    /// so phrase / proximity / highlight queries can verify adjacency.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("t", "d:", IndexKind::Text, ValType::Str).with_field("b").build()?;
    /// assert!(!s.has_positions());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn has_positions(&self) -> bool {
        self.with_positions
    }

    /// Hash fields stored per row (`VALUES`, text / range / unique), which
    /// `FILTER`, `SORT`, `DISTINCT` and `FACET` read.
    ///
    /// ```
    /// use kevy_index::{IndexKind, IndexSpec, ValType, ValueSpec};
    /// let s = IndexSpec::builder("t", "d:", IndexKind::Text, ValType::Str)
    ///     .with_field("b")
    ///     .with_values(vec![ValueSpec::new("year").with_type(ValType::I64)])
    ///     .build()?;
    /// assert_eq!(s.values()[0].ty, ValType::I64);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn values(&self) -> &[ValueSpec] {
        &self.values
    }

    /// Composite columns: `Some` on an ORDERPATH-compiled index, whose
    /// value is the order-preserving concatenation of these columns'
    /// encodings ([`composite_encode`](crate::composite_encode)).
    ///
    /// ```
    /// use kevy_index::{CompositeCol, IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("t.by", "t:", IndexKind::Range, ValType::Str)
    ///     .with_field("a")
    ///     .with_composite(vec![CompositeCol::new("a", ValType::Str), CompositeCol::new("n", ValType::I64)])
    ///     .build()?;
    /// assert_eq!(s.composite().map(<[_]>::len), Some(2));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn composite(&self) -> Option<&[CompositeCol]> {
        self.composite.as_deref()
    }

    /// What this index reads out of one row: each declared field's raw
    /// bytes with its BM25 weight, and each declared `VALUES` field's raw
    /// bytes (`None` where the row has none).
    ///
    /// `get` fetches a hash field, so this stays free of any storage
    /// dependency while keeping the answer in one place — the server and
    /// the embedded store index the same row the same way by
    /// construction, rather than by two copies of the same loop agreeing.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64).with_field("f").build()?;
    /// let (fields, values) = s.read_row(|f| (f == b"f").then(|| b"7".to_vec()));
    /// assert_eq!((fields, values.len()), (vec![(b"7".to_vec(), 1.0)], 0));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn read_row(&self, mut get: impl FnMut(&[u8]) -> Option<Vec<u8>>) -> RowInputs {
        let mut fields = Vec::with_capacity(self.fields.len());
        for f in &self.fields {
            if let Some(raw) = get(&f.name) {
                fields.push((raw, f.weight));
            }
        }
        let values = self.values.iter().map(|v| get(&v.name)).collect();
        (fields, values)
    }
}
