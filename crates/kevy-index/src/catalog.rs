//! [`Catalog`] — the index registry: declarations, states, and the
//! compiled prefix matcher the write-path hook consults.

use crate::value::IndexValue;

/// Declared scalar type of an index (`TYPE i64|f64|str`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    /// v2.8: f32 LE vector blob (ANN kinds parse the field
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
    /// Point lookup by value; duplicates recorded (declarative fence —
    /// RFC D3: uniqueness is verified, not write-enforced).
    Unique,
    /// v2.7 full-text: the field tokenizes into an inverted segment
    /// (kevy-text); queried with `MATCH`, BM25-ranked.
    Text,
    /// v2.8 ANN: the field holds an f32 LE vector indexed in an HNSW
    /// graph (kevy-vector); queried with `KNN`, distance-ranked.
    Ann,
    /// v3.1 aggregate: per-group count/sum/min/max of the field,
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

/// Lifecycle state (RFC D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    /// Backfill in progress; queries answer `-INDEXBUILDING`.
    Building,
    /// Serving.
    Ready,
    /// Build aborted over budget (RFC D7); queries answer an error.
    FailedOverBudget,
}

/// One declared index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSpec {
    /// Unique catalog name.
    pub name: Vec<u8>,
    /// Key-prefix domain (`ON PREFIX user:`).
    pub prefix: Vec<u8>,
    /// Hash field the value comes from.
    pub field: Vec<u8>,
    /// Declared scalar type.
    pub ty: ValType,
    /// Range or unique.
    pub kind: IndexKind,
    /// Optional per-index byte budget (`MAXMEM`); 0 = unlimited.
    pub max_bytes: u64,
    /// v2.8: ANN parameters (`Some` iff kind == Ann).
    pub ann: Option<AnnSpec>,
    /// v3.1: grouping field (`Some` iff kind == Agg).
    pub group_by: Option<Vec<u8>>,
}

/// v2.8 — HNSW declaration (immutable once created; RFC D2).
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

/// Hard cap on declared indexes (RFC D1).
pub const MAX_INDEXES: usize = 64;

/// The registry. The runtime holds one per process behind an RCU-style
/// swap; shards read their clone lock-free.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    specs: Vec<(IndexSpec, IndexState)>,
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

    /// Serialize to the sidecar text form (one line per index:
    /// `name<TAB>prefix<TAB>field<TAB>ty<TAB>kind<TAB>max_bytes[<TAB>ann]`,
    /// fields hex-escaped for tabs/newlines via `%XX`; the 7th column
    /// is `dim,distance,m,ef` for ANN kinds).
    pub fn to_sidecar(&self) -> String {
        let mut out = String::from("kevy-index-catalog v1\n");
        for (s, _) in &self.specs {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                esc(&s.name),
                esc(&s.prefix),
                esc(&s.field),
                s.ty.tag(),
                s.kind.tag(),
                s.max_bytes
            ));
            // 7th column is kind-interpreted: ann params for Ann,
            // escaped group field for Agg.
            if let Some(a) = &s.ann {
                out.push_str(&format!("\t{},{},{},{}", a.dim, a.distance, a.m, a.ef));
            } else if let Some(g) = &s.group_by {
                out.push_str(&format!("\t{}", esc(g)));
            }
            out.push('\n');
        }
        out
    }

    /// Parse the sidecar text form; all indexes load as `Building`
    /// (boot rebuild, RFC D5). `None` on malformed input.
    pub fn from_sidecar(text: &str) -> Option<Catalog> {
        let mut lines = text.lines();
        if lines.next()? != "kevy-index-catalog v1" {
            return None;
        }
        let mut c = Catalog::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            c.create(spec_from_line(line)?).ok()?;
        }
        Some(c)
    }
}

/// Parse one sidecar line back into an [`IndexSpec`] — the inverse of
/// one `to_sidecar` line. `None` on malformed input.
fn spec_from_line(line: &str) -> Option<IndexSpec> {
    let parts: Vec<&str> = line.split('\t').collect();
    if !(parts.len() == 6 || parts.len() == 7) {
        return None;
    }
    let kind = IndexKind::parse(parts[4].as_bytes())?;
    let (ann, group_by) = if parts.len() == 7 {
        match kind {
            IndexKind::Ann => {
                let nums: Vec<&str> = parts[6].split(',').collect();
                if nums.len() != 4 {
                    return None;
                }
                (
                    Some(AnnSpec {
                        dim: nums[0].parse().ok()?,
                        distance: nums[1].parse().ok()?,
                        m: nums[2].parse().ok()?,
                        ef: nums[3].parse().ok()?,
                    }),
                    None,
                )
            }
            IndexKind::Agg => (None, Some(unesc(parts[6])?)),
            _ => return None,
        }
    } else {
        (None, None)
    };
    Some(IndexSpec {
        name: unesc(parts[0])?,
        prefix: unesc(parts[1])?,
        field: unesc(parts[2])?,
        ty: ValType::parse(parts[3].as_bytes())?,
        kind,
        max_bytes: parts[5].parse().ok()?,
        ann,
        group_by,
    })
}

fn esc(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for &c in b {
        if c == b'\t' || c == b'\n' || c == b'%' || !(32..127).contains(&c) {
            out.push_str(&format!("%{c:02X}"));
        } else {
            out.push(c as char);
        }
    }
    out
}

fn unesc(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, prefix: &str) -> IndexSpec {
        IndexSpec {
            name: name.into(),
            prefix: prefix.into(),
            field: b"age".to_vec(),
            ty: ValType::I64,
            kind: IndexKind::Range,
            ann: None,
            max_bytes: 0,
            group_by: None,
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
        s.field = b"f%\n".to_vec();
        s.max_bytes = 1024;
        c.create(s).unwrap();
        let text = c.to_sidecar();
        let c2 = Catalog::from_sidecar(&text).unwrap();
        let (got, st) = c2.get(b"weird").unwrap();
        assert_eq!(got.prefix, b"pre\tfix:".to_vec());
        assert_eq!(got.field, b"f%\n".to_vec());
        assert_eq!(got.max_bytes, 1024);
        assert_eq!(st, IndexState::Building, "boot loads as Building");
        assert!(Catalog::from_sidecar("bogus").is_none());
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
