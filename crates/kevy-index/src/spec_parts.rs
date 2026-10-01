//! The pieces an [`IndexSpec`](crate::IndexSpec) is declared from:
//! its fields, its stored values, its ANN parameters.

use crate::catalog::ValType;

/// One indexed attribute of a document.
///
/// `weight` scales this field's contribution to the BM25 score, so a hit
/// in a title can outrank one in a body. Weighting per field is exactly
/// what a per-field index cannot express: BM25 normalises by document
/// length, so separate indexes normalise over separate corpora and their
/// scores are not comparable. That is why multi-attribute is a struct
/// change rather than something a caller can assemble from several
/// single-field indexes.
///
/// ```
/// use kevy_index::FieldSpec;
/// let title = FieldSpec::new("title").with_weight(2.5);
/// assert_eq!((title.name.as_slice(), title.weight), (&b"title"[..], 2.5));
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FieldSpec {
    /// Hash field name.
    ///
    /// ```
    /// # use kevy_index::{FieldSpec, IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Text, ValType::Str)
    ///     .with_fields(vec![FieldSpec::new("title")])
    ///     .build()?;
    /// // the row's hash field of that name is what gets indexed
    /// let (fields, _) = s.read_row(|f| (f == s.fields()[0].name.as_slice()).then(|| b"hi".to_vec()));
    /// assert_eq!(fields, [(b"hi".to_vec(), 1.0)]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub name: Vec<u8>,
    /// BM25 weight; 1.0 is neutral.
    ///
    /// ```
    /// # use kevy_index::{FieldSpec, IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Text, ValType::Str)
    ///     .with_fields(vec![FieldSpec::new("title").with_weight(3.0)])
    ///     .build()?;
    /// // the weight rides with every value the field contributes
    /// let (fields, _) = s.read_row(|_| Some(b"x".to_vec()));
    /// assert_eq!(fields[0].1, s.fields()[0].weight);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub weight: f32,
}

impl FieldSpec {
    /// A neutrally-weighted field.
    ///
    /// ```
    /// assert_eq!(kevy_index::FieldSpec::new("body").weight, 1.0);
    /// ```
    pub fn new(name: impl Into<Vec<u8>>) -> FieldSpec {
        FieldSpec { name: name.into(), weight: 1.0 }
    }

    /// This field with BM25 weight `weight`.
    ///
    /// ```
    /// assert_eq!(kevy_index::FieldSpec::new("t").with_weight(3.0).weight, 3.0);
    /// ```
    #[must_use]
    pub fn with_weight(mut self, weight: f32) -> FieldSpec {
        self.weight = weight;
        self
    }
}

/// One stored value field: which hash field it reads, and how its bytes
/// compare.
///
/// The type is declared, not guessed per query. A numeric range compared
/// lexicographically is silently wrong — `"9"` sorts above `"10"` — and
/// deciding it by whether both sides happen to parse as a number would
/// make the answer depend on the data. Declaring it also means `SORT` and
/// `FACET` inherit an order and an identity rather than re-deciding one.
///
/// ```
/// use kevy_index::{ValType, ValueSpec};
/// let year = ValueSpec::new("year").with_type(ValType::I64);
/// assert_eq!(year.ty, ValType::I64);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ValueSpec {
    /// Hash field name.
    ///
    /// ```
    /// # use kevy_index::{IndexKind, IndexSpec, ValType, ValueSpec};
    /// let s = IndexSpec::builder("n", "p:", IndexKind::Range, ValType::I64)
    ///     .with_field("age")
    ///     .with_values(vec![ValueSpec::new("city")])
    ///     .build()?;
    /// let (_, values) = s.read_row(|f| (f == b"city").then(|| b"kyoto".to_vec()));
    /// assert_eq!((s.values()[0].name.as_slice(), &values[0]), (&b"city"[..], &Some(b"kyoto".to_vec())));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub name: Vec<u8>,
    /// How the stored bytes compare.
    ///
    /// ```
    /// use kevy_index::{ValType, ValueSpec, order_key};
    /// let as_text = ValueSpec::new("n");
    /// let as_number = ValueSpec::new("n").with_type(ValType::I64);
    /// // "9" sorts above "10" as text, below it as a number
    /// assert!(order_key(as_text.ty, b"9") > order_key(as_text.ty, b"10"));
    /// assert!(order_key(as_number.ty, b"9") < order_key(as_number.ty, b"10"));
    /// ```
    pub ty: ValType,
}

impl ValueSpec {
    /// A value field compared as text — the default when no type is
    /// declared for it.
    ///
    /// ```
    /// use kevy_index::{ValType, ValueSpec};
    /// assert_eq!(ValueSpec::new("tag").ty, ValType::Str);
    /// ```
    pub fn new(name: impl Into<Vec<u8>>) -> ValueSpec {
        ValueSpec { name: name.into(), ty: ValType::Str }
    }

    /// This value field compared as `ty`.
    ///
    /// ```
    /// use kevy_index::{ValType, ValueSpec};
    /// assert_eq!(ValueSpec::new("n").with_type(ValType::F64).ty, ValType::F64);
    /// ```
    #[must_use]
    pub fn with_type(mut self, ty: ValType) -> ValueSpec {
        self.ty = ty;
        self
    }
}

/// HNSW declaration (immutable once created).
///
/// ```
/// use kevy_index::AnnSpec;
/// let a = AnnSpec::new(128).with_distance(1).with_m(32);
/// assert_eq!((a.dim, a.distance, a.m, a.ef), (128, 1, 32, 200));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct AnnSpec {
    /// Vector dimensionality (field bytes must be dim×4 f32 LE).
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("v", "doc:", IndexKind::Ann, ValType::Vector)
    ///     .with_field("emb")
    ///     .with_ann(AnnSpec::new(3))
    ///     .build()?;
    /// let dim = s.ann().expect("an ann index").dim;
    /// // a stored vector is dim little-endian f32s
    /// let v: Vec<u8> = [0.5f32, 1.0, 2.0].iter().flat_map(|x| x.to_le_bytes()).collect();
    /// assert_eq!(v.len(), dim as usize * 4);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub dim: u32,
    /// 0=cosine 1=l2 2=ip (kevy-vector's Distance tags).
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("v", "doc:", IndexKind::Ann, ValType::Vector)
    ///     .with_field("emb")
    ///     .with_ann(AnnSpec::new(3).with_distance(1))
    ///     .build()?;
    /// assert_eq!(s.ann().map(|a| a.distance), Some(1), "l2");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub distance: u8,
    /// Max links per node per layer.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("v", "doc:", IndexKind::Ann, ValType::Vector)
    ///     .with_field("emb")
    ///     .build();
    /// // the parameters are required on an ann index
    /// assert!(s.is_err());
    /// assert_eq!(kevy_index::AnnSpec::new(3).m, 16, "the IDX.CREATE default");
    /// ```
    pub m: u16,
    /// Construction beam width.
    ///
    /// ```
    /// # use kevy_index::{AnnSpec, IndexKind, IndexSpec, ValType};
    /// let s = IndexSpec::builder("v", "doc:", IndexKind::Ann, ValType::Vector)
    ///     .with_field("emb")
    ///     .with_ann(AnnSpec::new(3).with_ef(64))
    ///     .build()?;
    /// assert_eq!(s.ann().map(|a| a.ef), Some(64));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub ef: u16,
}

impl AnnSpec {
    /// A `dim`-dimensional cosine graph with the HNSW defaults
    /// (`M 16`, `EF 200`), the values `IDX.CREATE` uses when the options
    /// are not given.
    ///
    /// ```
    /// let a = kevy_index::AnnSpec::new(4);
    /// assert_eq!((a.distance, a.m, a.ef), (0, 16, 200));
    /// ```
    #[must_use]
    pub fn new(dim: u32) -> AnnSpec {
        AnnSpec { dim, distance: 0, m: 16, ef: 200 }
    }

    /// This graph under distance tag `distance` (0 cosine, 1 l2, 2 ip).
    ///
    /// ```
    /// assert_eq!(kevy_index::AnnSpec::new(4).with_distance(2).distance, 2);
    /// ```
    #[must_use]
    pub fn with_distance(mut self, distance: u8) -> AnnSpec {
        self.distance = distance;
        self
    }

    /// This graph with at most `m` links per node per layer.
    ///
    /// ```
    /// assert_eq!(kevy_index::AnnSpec::new(4).with_m(8).m, 8);
    /// ```
    #[must_use]
    pub fn with_m(mut self, m: u16) -> AnnSpec {
        self.m = m;
        self
    }

    /// This graph built with beam width `ef`.
    ///
    /// ```
    /// assert_eq!(kevy_index::AnnSpec::new(4).with_ef(64).ef, 64);
    /// ```
    #[must_use]
    pub fn with_ef(mut self, ef: u16) -> AnnSpec {
        self.ef = ef;
        self
    }
}
