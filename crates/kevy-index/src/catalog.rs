//! [`Catalog`] — the index registry: declarations, states, and the
//! compiled prefix matcher the write-path hook consults.

use crate::value::IndexValue;

/// Declared scalar type of an index (`TYPE i64|f64|str`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    /// f32 LE vector blob (ANN kinds parse the field
    /// themselves — never coerced through IndexValue).
    Vector,
    /// Signed 64-bit integer.
    I64,
    /// Finite 64-bit float (NaN coerce-fails).
    F64,
    /// Raw bytes, memcmp order.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// Ordered scan over `(value, key)` pairs.
    Range,
    /// Point lookup by value; duplicates recorded (declarative fence:
    /// uniqueness is verified, not write-enforced).
    Unique,
    /// Full-text: the field tokenizes into an inverted segment
    /// (kevy-text); queried with `MATCH`, BM25-ranked.
    Text,
    /// ANN: the field holds an f32 LE vector indexed in an HNSW
    /// graph (kevy-vector); queried with `KNN`, distance-ranked.
    Ann,
    /// Aggregate: per-group count/sum/min/max of the field,
    /// grouped by `IndexSpec::group_by`; queried with `GROUP`/`GROUPS`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    /// Backfill in progress; queries answer `-INDEXBUILDING`.
    Building,
    /// Serving.
    Ready,
    /// Build aborted over budget; queries answer an error.
    FailedOverBudget,
}

/// One indexed attribute of a document.
///
/// `weight` scales this field's contribution to the BM25 score, so a hit
/// in a title can outrank one in a body. Weighting per field is exactly
/// what a per-field index cannot express: BM25 normalises by document
/// length, so separate indexes normalise over separate corpora and their
/// scores are not comparable. That is why multi-attribute is a struct
/// change rather than something a caller can assemble from several
/// single-field indexes.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSpec {
    /// Hash field name.
    pub name: Vec<u8>,
    /// BM25 weight; 1.0 is neutral.
    pub weight: f32,
}

impl FieldSpec {
    /// A neutrally-weighted field.
    pub fn new(name: impl Into<Vec<u8>>) -> FieldSpec {
        FieldSpec { name: name.into(), weight: 1.0 }
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
#[derive(Debug, Clone, PartialEq)]
pub struct ValueSpec {
    /// Hash field name.
    pub name: Vec<u8>,
    /// How the stored bytes compare.
    pub ty: ValType,
}

impl ValueSpec {
    /// A value field compared as text — the default when no type is
    /// declared for it.
    pub fn new(name: impl Into<Vec<u8>>) -> ValueSpec {
        ValueSpec { name: name.into(), ty: ValType::Str }
    }
}

// `ValueTest` (the stored-value comparison) lives in `value.rs` with
// the rest of the coercion/order logic; re-exported unchanged.

/// One declared index.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSpec {
    /// Unique catalog name.
    pub name: Vec<u8>,
    /// Key-prefix domain (`ON PREFIX user:`).
    pub prefix: Vec<u8>,
    /// Hash fields the value comes from, in declaration order.
    ///
    /// No single-field twin is kept alongside this: two sources of truth
    /// for "which field" is the shape that drifts. Single-field indexes
    /// are the one-element case, read through [`IndexSpec::field`].
    pub fields: Vec<FieldSpec>,
    /// Declared scalar type.
    pub ty: ValType,
    /// Range or unique.
    pub kind: IndexKind,
    /// Optional per-index byte budget (`MAXMEM`); 0 = unlimited.
    pub max_bytes: u64,
    /// ANN parameters (`Some` iff kind == Ann).
    pub ann: Option<AnnSpec>,
    /// Grouping field (`Some` iff kind == Agg).
    pub group_by: Option<Vec<u8>>,
    /// Record token positions (`WITH POSITIONS`, kind == Text only), so
    /// phrase / proximity / highlight queries can verify adjacency. Off
    /// by default: a corpus that never runs a phrase query does not pay
    /// the positional side-channel's memory.
    pub with_positions: bool,
    /// Hash fields stored per document (`VALUES`, kind == Text only), so
    /// the clauses that read a document's own value — `FILTER` and, in
    /// time, `SORT` / `DISTINCT` / `FACET` — have something to read.
    /// Empty by default: an index that never filters does not pay for
    /// the stored column.
    pub values: Vec<ValueSpec>,
    /// Composite columns (`Some` = an ORDERPATH-compiled index): the
    /// index value is the order-preserving concatenation of these
    /// columns' encodings (see [`crate::composite`]). Legal ONLY on
    /// `KIND range` with `TYPE str`; `None` = every existing index,
    /// byte-identical in memory and on the sidecar (A5).
    pub composite: Option<Vec<crate::composite::CompositeCol>>,
}

/// HNSW declaration (immutable once created).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnSpec {
    /// Vector dimensionality (field bytes must be dim×4 f32 LE).
    pub dim: u32,
    /// 0=cosine 1=l2 2=ip (kevy-vector's Distance tags).
    pub distance: u8,
    /// Max links per node per layer.
    pub m: u16,
    /// Construction beam width.
    pub ef: u16,
}

/// Hard cap on declared indexes.
pub const MAX_INDEXES: usize = 64;

/// The registry. The runtime holds one per process behind an RCU-style
/// swap; shards read their clone lock-free.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub(crate) specs: Vec<(IndexSpec, IndexState)>,
}

/// What one row looks like to an index: each declared field's raw bytes
/// with its BM25 weight, and each declared `VALUES` field's raw bytes
/// (`None` where the row has none).
pub type RowInputs = (Vec<(Vec<u8>, f32)>, Vec<Option<Vec<u8>>>);

impl IndexSpec {
    /// The primary field — the first declared one. Every kind except
    /// text indexes exactly one attribute; text is the kind that reads
    /// What this index reads out of one row: each declared field's raw
    /// bytes with its BM25 weight, and each declared `VALUES` field's raw
    /// bytes (`None` where the row has none).
    ///
    /// `get` fetches a hash field, so this stays free of any storage
    /// dependency while keeping the answer in one place — the server and
    /// the embedded store index the same row the same way by
    /// construction, rather than by two copies of the same loop agreeing.
    pub fn read_row(
        &self,
        mut get: impl FnMut(&[u8]) -> Option<Vec<u8>>,
    ) -> RowInputs {
        let mut fields = Vec::with_capacity(self.fields.len());
        for f in &self.fields {
            if let Some(raw) = get(&f.name) {
                fields.push((raw, f.weight));
            }
        }
        let values = self.values.iter().map(|v| get(&v.name)).collect();
        (fields, values)
    }

    /// [`IndexSpec::fields`] in full.
    pub fn field(&self) -> &[u8] {
        self.fields.first().map_or(&[][..], |f| f.name.as_slice())
    }

    /// Declare a single-field index — the shape every kind but text uses.
    pub fn single_field(
        name: Vec<u8>,
        prefix: Vec<u8>,
        field: Vec<u8>,
        ty: ValType,
        kind: IndexKind,
    ) -> IndexSpec {
        IndexSpec {
            name,
            prefix,
            fields: vec![FieldSpec::new(field)],
            ty,
            kind,
            max_bytes: 0,
            ann: None,
            group_by: None,
            with_positions: false,
            values: Vec::new(),
            composite: None,
        }
    }
}

impl Catalog {
    /// Empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new index. Errors on duplicate name / cap.
    pub fn create(&mut self, spec: IndexSpec) -> Result<(), &'static str> {
        if self.specs.len() >= MAX_INDEXES {
            return Err("ERR index limit reached (64)");
        }
        if self.specs.iter().any(|(s, _)| s.name == spec.name) {
            return Err("ERR index already exists");
        }
        if spec.fields.is_empty() {
            return Err("ERR index needs at least one field");
        }
        // Multi-field is served by the text engine only. Every other
        // kind reads one scalar, so a second field on a range or unique
        // index would be declared and never consulted -- the
        // accept-and-ignore shape this arc keeps refusing.
        if spec.fields.len() > 1 && spec.kind != IndexKind::Text {
            return Err("ERR only KIND text indexes several fields");
        }
        // Positions are a text-only capability: phrase / proximity /
        // highlight all read the positional side-channel, which no other
        // kind maintains, so accepting the flag elsewhere would be the
        // accept-and-ignore shape this arc keeps refusing.
        if spec.with_positions && spec.kind != IndexKind::Text {
            return Err("ERR WITH POSITIONS requires KIND text");
        }
        // VALUES rides the kinds that carry a stored-value column: the
        // text segment and the scalar segments (range / unique — the
        // capacity arc's G1 generalization). Ann and agg carry none, so
        // accepting the declaration there would store nothing and
        // filter on nothing.
        if !spec.values.is_empty()
            && !matches!(spec.kind, IndexKind::Text | IndexKind::Range | IndexKind::Unique)
        {
            return Err("ERR VALUES requires KIND text|range|unique");
        }
        // Composite (ORDERPATH-compiled) combos: named refusals, body
        // in `composite.rs` beside the encoding it protects.
        crate::composite::composite_guard(&spec)?;
        self.specs.push((spec, IndexState::Building));
        Ok(())
    }

    /// Drop by name; `false` if absent.
    pub fn drop_index(&mut self, name: &[u8]) -> bool {
        let before = self.specs.len();
        self.specs.retain(|(s, _)| s.name != name);
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
    pub fn matching<'a>(&'a self, key: &'a [u8]) -> impl Iterator<Item = (&'a IndexSpec, IndexState)> {
        self.specs
            .iter()
            .filter(move |(s, _)| key.starts_with(&s.prefix))
            .map(|(s, st)| (s, *st))
    }

    /// Coerce a raw field value for `spec` (convenience passthrough).
    pub fn coerce(spec: &IndexSpec, raw: &[u8]) -> Option<IndexValue> {
        IndexValue::coerce(spec.ty, raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, prefix: &str) -> IndexSpec {
        IndexSpec {
            name: name.into(),
            prefix: prefix.into(),
            fields: vec![FieldSpec::new(b"age".to_vec())],
            ty: ValType::I64,
            kind: IndexKind::Range,
            ann: None,
            max_bytes: 0,
            group_by: None,
            with_positions: false,
            values: Vec::new(),
            composite: None,
        }
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
        let mut s = spec("weird", "pre\tfix:");
        s.fields = vec![FieldSpec::new(b"f%\n".to_vec())];
        s.max_bytes = 1024;
        c.create(s).unwrap();
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
        let two = || {
            vec![
                FieldSpec::new(b"title".to_vec()),
                FieldSpec::new(b"body".to_vec()),
            ]
        };
        let mut range = spec("multi-range", "p:");
        range.fields = two();
        assert!(Catalog::new().create(range).is_err(), "range must refuse two fields");

        let mut text = spec("multi-text", "p:");
        text.kind = IndexKind::Text;
        text.fields = two();
        assert!(Catalog::new().create(text).is_ok(), "text must accept them");
    }

    #[test]
    fn an_index_needs_at_least_one_field() {
        let mut s = spec("nofields", "p:");
        s.fields.clear();
        assert!(Catalog::new().create(s).is_err());
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
