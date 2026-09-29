//! [`Catalog`] — the index registry: declarations, states, and the
//! compiled prefix matcher the write-path hook consults.

use crate::error::{CatalogError, Declared};
use crate::spec::IndexSpec;

/// Declared scalar type of an index (`TYPE i64|f64|str`).
///
/// ```
/// use kevy_index::ValType;
/// assert_eq!(ValType::parse(b"F64"), Some(ValType::F64));
/// assert_eq!(ValType::parse(ValType::Str.tag().as_bytes()), Some(ValType::Str));
/// assert_eq!(ValType::parse(b"u8"), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ValType {
    /// f32 LE vector blob (ANN kinds parse the field
    /// themselves — never coerced through IndexValue).
    ///
    /// ```
    /// use kevy_index::{AnnSpec, IndexKind, IndexSpec, IndexValue, ValType};
    /// let knn = IndexSpec::builder("emb", "doc:", IndexKind::Ann, ValType::Vector)
    ///     .with_field("emb")
    ///     .with_ann(AnnSpec::new(3))
    ///     .build()?;
    /// assert_eq!(knn.ty(), ValType::Vector);
    /// assert_eq!(IndexValue::coerce(ValType::Vector, b"\0\0\0\0"), None, "never a scalar");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Vector,
    /// Signed 64-bit integer.
    ///
    /// ```
    /// use kevy_index::{IndexValue, ValType};
    /// assert_eq!(IndexValue::coerce(ValType::I64, b"-7"), Some(IndexValue::I64(-7)));
    /// assert_eq!(IndexValue::coerce(ValType::I64, b"7.5"), None);
    /// ```
    I64,
    /// Finite 64-bit float (NaN coerce-fails).
    ///
    /// ```
    /// use kevy_index::{IndexValue, ValType};
    /// assert_eq!(IndexValue::coerce(ValType::F64, b"7.5"), Some(IndexValue::F64(7.5)));
    /// assert_eq!(IndexValue::coerce(ValType::F64, b"nan"), None);
    /// ```
    F64,
    /// Raw bytes, memcmp order.
    ///
    /// ```
    /// use kevy_index::{IndexValue, ValType};
    /// let (a, b) = (IndexValue::coerce(ValType::Str, b"B"), IndexValue::coerce(ValType::Str, b"a"));
    /// assert!(a < b, "bytewise: 'B' (0x42) sorts before 'a' (0x61)");
    /// ```
    Str,
}

impl ValType {
    /// Wire tag (catalog sidecar + IDX.LIST).
    pub fn tag(self) -> &'static str {
        match self {
            ValType::I64 => "i64",
            ValType::F64 => "f64",
            ValType::Str => "str",
            ValType::Vector => "vector",
        }
    }

    /// Parse a wire tag.
    pub fn parse(raw: &[u8]) -> Option<ValType> {
        if raw.eq_ignore_ascii_case(b"i64") {
            Some(ValType::I64)
        } else if raw.eq_ignore_ascii_case(b"f64") {
            Some(ValType::F64)
        } else if raw.eq_ignore_ascii_case(b"str") {
            Some(ValType::Str)
        } else if raw.eq_ignore_ascii_case(b"vector") {
            Some(ValType::Vector)
        } else {
            None
        }
    }
}

/// Index kind (`KIND range|unique`).
///
/// ```
/// use kevy_index::IndexKind;
/// assert_eq!(IndexKind::parse(b"UNIQUE"), Some(IndexKind::Unique));
/// assert_eq!(IndexKind::parse(IndexKind::Agg.tag().as_bytes()), Some(IndexKind::Agg));
/// assert_eq!(IndexKind::parse(b"hash"), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexKind {
    /// Ordered scan over `(value, key)` pairs.
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut seg = Segment::new();
    /// seg.apply(b"u:1", Some(IndexValue::I64(30)));
    /// seg.apply(b"u:2", Some(IndexValue::I64(20)));
    /// let (hits, _) = seg.range(&IndexValue::I64(0), &IndexValue::I64(99), None, 10);
    /// assert_eq!(hits[0].0, b"u:2", "value order, not key order");
    /// ```
    Range,
    /// Point lookup by value; duplicates recorded (declarative fence:
    /// uniqueness is verified, not write-enforced).
    ///
    /// ```
    /// use kevy_index::{IndexValue, Segment};
    /// let mut seg = Segment::new();
    /// seg.apply(b"u:1", Some(IndexValue::Str(b"a@x".to_vec())));
    /// seg.apply(b"u:2", Some(IndexValue::Str(b"a@x".to_vec())));
    /// // the second write is not refused; the lookup shows the duplicate
    /// assert_eq!(seg.eq(&IndexValue::Str(b"a@x".to_vec()), 10).len(), 2);
    /// ```
    Unique,
    /// Full-text: the field tokenizes into an inverted segment
    /// (kevy-text); queried with `MATCH`, BM25-ranked.
    ///
    /// ```
    /// use kevy_index::{FieldSpec, IndexKind, IndexSpec, ValType};
    /// let fields = vec![FieldSpec::new("title").with_weight(2.0), FieldSpec::new("body")];
    /// let text = IndexSpec::builder("posts", "post:", IndexKind::Text, ValType::Str)
    ///     .with_fields(fields)
    ///     .build()?;
    /// assert_eq!(text.fields().len(), 2, "only text serves several fields");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Text,
    /// ANN: the field holds an f32 LE vector indexed in an HNSW
    /// graph (kevy-vector); queried with `KNN`, distance-ranked.
    ///
    /// ```
    /// use kevy_index::{AnnSpec, IndexKind, IndexSpec, ValType};
    /// let b = IndexSpec::builder("emb", "doc:", IndexKind::Ann, ValType::Vector).with_field("v");
    /// assert!(b.clone().build().is_err(), "an ANN index needs its parameters");
    /// assert_eq!(b.with_ann(AnnSpec::new(128)).build()?.ann(), Some(AnnSpec::new(128)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Ann,
    /// Aggregate: per-group count/sum/min/max of the field,
    /// grouped by `IndexSpec::group_by`; queried with `GROUP`/`GROUPS`.
    ///
    /// ```
    /// use kevy_index::{IndexKind, IndexSpec, ValType};
    /// let spend = IndexSpec::builder("spend", "o:", IndexKind::Agg, ValType::F64)
    ///     .with_field("total")
    ///     .with_group_by("region")
    ///     .build()?;
    /// assert_eq!(spend.group_by(), Some(&b"region"[..]));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Agg,
}

impl IndexKind {
    /// Wire tag.
    pub fn tag(self) -> &'static str {
        match self {
            IndexKind::Range => "range",
            IndexKind::Unique => "unique",
            IndexKind::Text => "text",
            IndexKind::Ann => "ann",
            IndexKind::Agg => "agg",
        }
    }

    /// Parse a wire tag.
    pub fn parse(raw: &[u8]) -> Option<IndexKind> {
        if raw.eq_ignore_ascii_case(b"range") {
            Some(IndexKind::Range)
        } else if raw.eq_ignore_ascii_case(b"unique") {
            Some(IndexKind::Unique)
        } else if raw.eq_ignore_ascii_case(b"text") {
            Some(IndexKind::Text)
        } else if raw.eq_ignore_ascii_case(b"ann") {
            Some(IndexKind::Ann)
        } else if raw.eq_ignore_ascii_case(b"agg") {
            Some(IndexKind::Agg)
        } else {
            None
        }
    }
}

/// Lifecycle state.
///
/// ```
/// use kevy_index::{Catalog, IndexKind, IndexSpec, IndexState, ValType};
/// let mut c = Catalog::new();
/// c.create(IndexSpec::builder("age", "u:", IndexKind::Range, ValType::I64).with_field("age").build()?)?;
/// assert_eq!(c.get(b"age").map(|(_, st)| st), Some(IndexState::Building));
/// assert!(c.set_state(b"age", IndexState::Ready));
/// assert_eq!(c.get(b"age").map(|(_, st)| st), Some(IndexState::Ready));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexState {
    /// Backfill in progress; queries answer `-INDEXBUILDING`.
    ///
    /// ```
    /// use kevy_index::{Catalog, IndexKind, IndexSpec, IndexState, ValType};
    /// let mut c = Catalog::new();
    /// c.create(IndexSpec::builder("age", "u:", IndexKind::Range, ValType::I64).with_field("age").build()?)?;
    /// assert_eq!(c.iter().next().map(|(_, st)| st), Some(IndexState::Building), "every index starts here");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Building,
    /// Serving.
    ///
    /// ```
    /// use kevy_index::{Catalog, IndexKind, IndexSpec, IndexState, ValType};
    /// let mut c = Catalog::new();
    /// c.create(IndexSpec::builder("age", "u:", IndexKind::Range, ValType::I64).with_field("age").build()?)?;
    /// c.set_state(b"age", IndexState::Ready); // backfill finished
    /// let serving = c.matching(b"u:1").filter(|(_, st)| *st == IndexState::Ready).count();
    /// assert_eq!(serving, 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Ready,
    /// Build aborted over budget; queries answer an error.
    ///
    /// ```
    /// use kevy_index::{Catalog, IndexKind, IndexSpec, IndexState, ValType};
    /// let mut c = Catalog::new();
    /// let spec = IndexSpec::builder("age", "u:", IndexKind::Range, ValType::I64).with_field("age");
    /// c.create(spec.with_max_bytes(4096).build()?)?;
    /// // the runtime's backfill passed max_bytes and gave up
    /// c.set_state(b"age", IndexState::FailedOverBudget);
    /// assert_eq!(c.get(b"age").map(|(_, st)| st), Some(IndexState::FailedOverBudget));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    FailedOverBudget,
}

/// Hard cap on declared indexes.
pub const MAX_INDEXES: usize = 64;

/// The registry. The runtime holds one per process behind an RCU-style
/// swap; shards read their clone lock-free.
///
/// ```
/// use kevy_index::{Catalog, CatalogError, IndexKind, IndexSpec, ValType};
/// let spec = |name: &str, prefix: &str| {
///     IndexSpec::builder(name, prefix, IndexKind::Range, ValType::I64).with_field("n").build()
/// };
/// let mut c = Catalog::new();
/// assert!(c.is_empty());
/// c.create(spec("age", "user:")?)?;
/// c.create(spec("ttl", "sess:")?)?;
/// assert!(matches!(c.create(spec("age", "x:")?), Err(CatalogError::Exists(_))));
/// assert_eq!(c.len(), 2);
/// let hit: Vec<_> = c.matching(b"user:7").map(|(s, _)| s.name().to_vec()).collect();
/// assert_eq!(hit, [b"age".to_vec()]);
/// assert_eq!(c.get(b"ttl").map(|(s, _)| s.prefix()), Some(&b"sess:"[..]));
/// assert!(c.drop_index(b"ttl") && !c.drop_index(b"ttl"));
/// assert_eq!(c.iter().count(), 1);
///
/// // the sidecar text restores the declarations, every one as Building
/// let back = Catalog::from_sidecar(&c.to_sidecar()).ok_or("sidecar did not parse")?;
/// assert_eq!(back.get(b"age").map(|(s, _)| s.name()), Some(&b"age"[..]));
/// assert!(Catalog::from_sidecar("not a sidecar").is_none());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub(crate) specs: Vec<(IndexSpec, IndexState)>,
    /// The global indexes' partitionings, by name; any other is local.
    pub(crate) parts: Vec<(Vec<u8>, crate::Partitioning)>,
}

impl Catalog {
    /// Empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new index. Errors on duplicate name / cap; the spec
    /// itself is consistent by construction.
    pub fn create(&mut self, spec: IndexSpec) -> Result<(), CatalogError> {
        if self.specs.len() >= MAX_INDEXES {
            return Err(CatalogError::Full(Declared::Index));
        }
        if self.specs.iter().any(|(s, _)| s.name == spec.name) {
            return Err(CatalogError::Exists(Declared::Index));
        }
        self.specs.push((spec, IndexState::Building));
        Ok(())
    }

    /// Drop by name; `false` if absent.
    pub fn drop_index(&mut self, name: &[u8]) -> bool {
        let before = self.specs.len();
        self.specs.retain(|(s, _)| s.name != name);
        self.parts.retain(|(n, _)| n != name);
        self.specs.len() != before
    }

    /// Set an index's lifecycle state; `false` if absent.
    pub fn set_state(&mut self, name: &[u8], state: IndexState) -> bool {
        for (s, st) in &mut self.specs {
            if s.name == name {
                *st = state;
                return true;
            }
        }
        false
    }

    /// Look up by name.
    pub fn get(&self, name: &[u8]) -> Option<(&IndexSpec, IndexState)> {
        self.specs.iter().find(|(s, _)| s.name == name).map(|(s, st)| (s, *st))
    }

    /// All specs with states, declaration order.
    pub fn iter(&self) -> impl Iterator<Item = (&IndexSpec, IndexState)> {
        self.specs.iter().map(|(s, st)| (s, *st))
    }

    /// Number of declared indexes.
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// Whether no indexes are declared (the write hook's fast path).
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// The write-path matcher: indexes whose prefix domain contains
    /// `key`. Linear over ≤64 specs with a memcmp each — the compiled
    /// trie of the RFC becomes worthwhile only past this cap, so the
    /// simple form IS the fast form at our scale.
    pub fn matching<'a>(
        &'a self,
        key: &'a [u8],
    ) -> impl Iterator<Item = (&'a IndexSpec, IndexState)> {
        self.specs.iter().filter(move |(s, _)| key.starts_with(&s.prefix)).map(|(s, st)| (s, *st))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldSpec;

    fn spec(name: &str, prefix: &str) -> IndexSpec {
        builder(name, prefix).build().unwrap()
    }

    fn builder(name: &str, prefix: &str) -> crate::IndexSpecBuilder {
        IndexSpec::builder(name, prefix, IndexKind::Range, ValType::I64).with_field("age")
    }

    #[test]
    fn create_drop_match_lifecycle() {
        let mut c = Catalog::new();
        c.create(spec("a", "user:")).unwrap();
        c.create(spec("b", "sess:")).unwrap();
        assert!(c.create(spec("a", "x:")).is_err(), "dup name");
        assert_eq!(c.matching(b"user:42").count(), 1);
        assert_eq!(c.matching(b"other:1").count(), 0);
        assert_eq!(c.get(b"a").unwrap().1, IndexState::Building);
        assert!(c.set_state(b"a", IndexState::Ready));
        assert_eq!(c.get(b"a").unwrap().1, IndexState::Ready);
        assert!(c.drop_index(b"b"));
        assert!(!c.drop_index(b"b"));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn sidecar_roundtrip_with_escapes() {
        let mut c = Catalog::new();
        let s = IndexSpec::builder("weird", "pre\tfix:", IndexKind::Range, ValType::I64)
            .with_field(b"f%\n".to_vec())
            .with_max_bytes(1024);
        c.create(s.build().unwrap()).unwrap();
        let text = c.to_sidecar();
        let c2 = Catalog::from_sidecar(&text).unwrap();
        let (got, st) = c2.get(b"weird").unwrap();
        assert_eq!(got.prefix, b"pre\tfix:".to_vec());
        assert_eq!(got.field(), b"f%\n");
        assert_eq!(got.max_bytes, 1024);
        assert_eq!(st, IndexState::Building, "boot loads as Building");
        assert!(Catalog::from_sidecar("bogus").is_none());
    }

    /// Text serves several fields; every other kind reads one scalar,
    /// so a second field there would be declared and never consulted.
    /// Accept-and-ignore is the shape this refuses.
    #[test]
    fn only_text_indexes_accept_several_fields() {
        let two = || vec![FieldSpec::new(b"title".to_vec()), FieldSpec::new(b"body".to_vec())];
        let range = builder("multi-range", "p:").with_fields(two());
        assert!(range.build().is_err(), "range must refuse two fields");

        let text = IndexSpec::builder("multi-text", "p:", IndexKind::Text, ValType::Str);
        let text = text.with_fields(two()).build().expect("text must accept them");
        assert!(Catalog::new().create(text).is_ok());
    }

    #[test]
    fn an_index_needs_at_least_one_field() {
        let s = builder("nofields", "p:").with_fields(Vec::new());
        assert!(s.build().is_err());
    }

    #[test]
    fn cap_enforced() {
        let mut c = Catalog::new();
        for i in 0..MAX_INDEXES {
            c.create(spec(&format!("i{i}"), "p:")).unwrap();
        }
        assert!(c.create(spec("over", "p:")).is_err());
    }
}
