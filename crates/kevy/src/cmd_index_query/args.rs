//! IDX.* argv grammar: the query shapes and their parsers.

use kevy_index::{Cursor, IndexValue, ValType};

use super::wire::{decode_cursor, unhex};

pub(crate) enum Shape {
    Range { min: Vec<u8>, max: Vec<u8> },
    Eq { value: Vec<u8> },
    Verify,
}

/// One side of a COMPOSE (name + its shape).
pub(crate) struct SubQuery {
    pub(super) name: Vec<u8>,
    pub(super) shape: Shape,
}

pub(crate) struct Query {
    pub(crate) name: Vec<u8>,
    pub(crate) shape: Shape,
    pub(crate) limit: usize,
    pub(crate) cursor_raw: Option<Vec<u8>>,
    /// `FIELDS f…` hydration list (owning-shard hash reads ride the
    /// chunk; empty = keys/values only).
    pub(crate) fields: Vec<Vec<u8>>,
}

impl Query {
    /// `IDX.QUERY name RANGE min max [LIMIT n] [CURSOR c]`
    /// `IDX.QUERY name EQ v [LIMIT n] [CURSOR c]`
    /// `IDX.COUNT name RANGE min max` / `EQ v` / `IDX.VERIFY name`
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<Query> {
        let verb = argv.first()?;
        if verb.eq_ignore_ascii_case(b"IDX.VERIFY") {
            return Some(Query {
                name: argv.get(1)?.clone(),
                shape: Shape::Verify,
                limit: 0,
                cursor_raw: None,
                fields: Vec::new(),
            });
        }
        let name = argv.get(1)?.clone();
        let mode = argv.get(2)?;
        let (shape, mut i) = if mode.eq_ignore_ascii_case(b"RANGE") {
            (
                Shape::Range { min: argv.get(3)?.clone(), max: argv.get(4)?.clone() },
                5,
            )
        } else if mode.eq_ignore_ascii_case(b"EQ") {
            (Shape::Eq { value: argv.get(3)?.clone() }, 4)
        } else {
            return None;
        };
        let mut limit = 100usize;
        let mut cursor_raw = None;
        let mut fields = Vec::new();
        while i < argv.len() {
            let a = &argv[i];
            if a.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if a.eq_ignore_ascii_case(b"CURSOR") {
                cursor_raw = Some(argv.get(i + 1)?.clone());
                i += 2;
            } else if a.eq_ignore_ascii_case(b"FIELDS") {
                fields = argv[i + 1..].to_vec();
                if fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(Query { name, shape, limit: limit.clamp(1, 10_000), cursor_raw, fields })
    }

    pub(super) fn bounds(&self, ty: ValType) -> Option<(IndexValue, IndexValue)> {
        match &self.shape {
            Shape::Range { min, max } => Some((
                IndexValue::parse_literal(ty, min)?,
                IndexValue::parse_literal(ty, max)?,
            )),
            Shape::Eq { value } => {
                let v = IndexValue::parse_literal(ty, value)?;
                Some((v.clone(), v))
            }
            Shape::Verify => None,
        }
    }

    pub(super) fn cursor(&self, _ty: ValType) -> Option<Cursor> {
        self.cursor_raw.as_deref().and_then(decode_cursor)
    }
}

/// `IDX.QUERY COMPOSE AND|OR sub1 sub2 …` — key-ordered (the two
/// indexes' value domains differ, so composition orders by key and
/// the cursor is a plain key point).
pub(crate) struct ComposeQuery {
    pub(crate) and: bool,
    pub(crate) a: SubQuery,
    pub(crate) b: SubQuery,
    pub(crate) limit: usize,
    pub(crate) cursor_key: Option<Vec<u8>>,
    pub(crate) fields: Vec<Vec<u8>>,
}

impl ComposeQuery {
    /// `IDX.QUERY COMPOSE AND|OR nameA <shapeA> nameB <shapeB>
    /// [LIMIT n] [CURSOR k] [FIELDS f…]` where shape =
    /// `RANGE min max` | `EQ v`.
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<ComposeQuery> {
        if !argv.first()?.eq_ignore_ascii_case(b"IDX.QUERY")
            || !argv.get(1)?.eq_ignore_ascii_case(b"COMPOSE")
        {
            return None;
        }
        let mode = argv.get(2)?;
        let and = if mode.eq_ignore_ascii_case(b"AND") {
            true
        } else if mode.eq_ignore_ascii_case(b"OR") {
            false
        } else {
            return None;
        };
        let (a, i) = parse_sub(argv, 3)?;
        let (b, mut i) = parse_sub(argv, i)?;
        let mut limit = 100usize;
        let mut cursor_key = None;
        let mut fields = Vec::new();
        while i < argv.len() {
            let t = &argv[i];
            if t.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if t.eq_ignore_ascii_case(b"CURSOR") {
                let raw = argv.get(i + 1)?;
                cursor_key = if raw == b"0" { None } else { Some(unhex(raw)?) };
                i += 2;
            } else if t.eq_ignore_ascii_case(b"FIELDS") {
                fields = argv[i + 1..].to_vec();
                if fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(ComposeQuery { and, a, b, limit: limit.clamp(1, 10_000), cursor_key, fields })
    }
}

fn parse_sub(argv: &[Vec<u8>], i: usize) -> Option<(SubQuery, usize)> {
    let name = argv.get(i)?.clone();
    let mode = argv.get(i + 1)?;
    if mode.eq_ignore_ascii_case(b"RANGE") {
        Some((
            SubQuery {
                name,
                shape: Shape::Range { min: argv.get(i + 2)?.clone(), max: argv.get(i + 3)?.clone() },
            },
            i + 4,
        ))
    } else if mode.eq_ignore_ascii_case(b"EQ") {
        Some((SubQuery { name, shape: Shape::Eq { value: argv.get(i + 2)?.clone() } }, i + 3))
    } else {
        None
    }
}

pub(crate) struct MatchArgs {
    pub(crate) name: Vec<u8>,
    pub(crate) text: Vec<u8>,
    pub(crate) limit: usize,
    pub(crate) fields: Vec<Vec<u8>>,
}

impl MatchArgs {
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<MatchArgs> {
        let name = argv.get(1)?.clone();
        if !argv.get(2)?.eq_ignore_ascii_case(b"MATCH") {
            return None;
        }
        let text = argv.get(3)?.clone();
        let mut limit = 10usize;
        let mut fields = Vec::new();
        let mut i = 4;
        while i < argv.len() {
            let t = &argv[i];
            if t.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if t.eq_ignore_ascii_case(b"FIELDS") {
                fields = argv[i + 1..].to_vec();
                if fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(MatchArgs { name, text, limit: limit.clamp(1, 1000), fields })
    }
}

/// `IDX.QUERY name KNN vec [LIMIT k] [FIELDS f…]` (no cursor; k ≤
/// 1000 — same rationale as MATCH).
pub(crate) struct KnnArgs {
    pub(crate) name: Vec<u8>,
    pub(crate) vec: Vec<u8>,
    pub(crate) limit: usize,
    /// Query beam width (`EF`); 0 = engine default.
    pub(crate) ef: usize,
    pub(crate) fields: Vec<Vec<u8>>,
}

impl KnnArgs {
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<KnnArgs> {
        let name = argv.get(1)?.clone();
        if !argv.get(2)?.eq_ignore_ascii_case(b"KNN") {
            return None;
        }
        let vec = argv.get(3)?.clone();
        let mut limit = 10usize;
        let mut ef = 0usize;
        let mut fields = Vec::new();
        let mut i = 4;
        while i < argv.len() {
            let t = &argv[i];
            if t.eq_ignore_ascii_case(b"LIMIT") {
                limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if t.eq_ignore_ascii_case(b"EF") {
                ef = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                if !(16..=4096).contains(&ef) {
                    return None;
                }
                i += 2;
            } else if t.eq_ignore_ascii_case(b"FIELDS") {
                fields = argv[i + 1..].to_vec();
                if fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(KnnArgs { name, vec, limit: limit.clamp(1, 1000), ef, fields })
    }
}

/// `IDX.QUERY name MATCH text [LIMIT n] [FIELDS f…]` (no cursor —
/// BM25 deep pagination is an anti-pattern; LIMIT caps at 1000).
/// `IDX.QUERY HYBRID <text_idx> MATCH <q> <ann_idx> KNN <vec>
/// [LIMIT n] [RRFK k] [EF ef] [FIELDS f…]` — reciprocal-rank fusion
/// of a BM25 list and a KNN list over the same prefix.
pub(crate) struct HybridArgs {
    pub(crate) text_idx: Vec<u8>,
    pub(crate) text: Vec<u8>,
    pub(crate) ann_idx: Vec<u8>,
    pub(crate) vec: Vec<u8>,
    pub(crate) limit: usize,
    pub(crate) rrf_k: f64,
    pub(crate) ef: usize,
    pub(crate) fields: Vec<Vec<u8>>,
}

impl HybridArgs {
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<HybridArgs> {
        if !argv.get(1)?.eq_ignore_ascii_case(b"HYBRID")
            || !argv.get(3)?.eq_ignore_ascii_case(b"MATCH")
            || !argv.get(6)?.eq_ignore_ascii_case(b"KNN")
        {
            return None;
        }
        let mut a = HybridArgs {
            text_idx: argv.get(2)?.clone(),
            text: argv.get(4)?.clone(),
            ann_idx: argv.get(5)?.clone(),
            vec: argv.get(7)?.clone(),
            limit: 10,
            rrf_k: 60.0,
            ef: 0,
            fields: Vec::new(),
        };
        let mut i = 8;
        while i < argv.len() {
            let t = &argv[i];
            if t.eq_ignore_ascii_case(b"LIMIT") {
                a.limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                if !(1..=1000).contains(&a.limit) {
                    return None;
                }
                i += 2;
            } else if t.eq_ignore_ascii_case(b"RRFK") {
                a.rrf_k = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                if !a.rrf_k.is_finite() || a.rrf_k <= 0.0 {
                    return None;
                }
                i += 2;
            } else if t.eq_ignore_ascii_case(b"EF") {
                a.ef = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                if !(16..=4096).contains(&a.ef) {
                    return None;
                }
                i += 2;
            } else if t.eq_ignore_ascii_case(b"FIELDS") {
                a.fields = argv[i + 1..].to_vec();
                if a.fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(a)
    }
}

/// `GROUPS [BY m] [LIMIT n]` tail — shared by op and reduce.
pub(crate) fn parse_groups_args(argv: &[Vec<u8>]) -> Option<(kevy_index::AggBy, usize)> {
    let (mut by, mut limit) = (kevy_index::AggBy::Count, 100usize);
    let mut i = 3;
    while i < argv.len() {
        if argv[i].eq_ignore_ascii_case(b"BY") {
            by = kevy_index::AggBy::parse(argv.get(i + 1)?)?;
            i += 2;
        } else if argv[i].eq_ignore_ascii_case(b"LIMIT") {
            limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            i += 2;
        } else if argv[i].starts_with(b"DEPTH=") {
            i += 1; // internal iterative-deepening marker
        } else {
            return None;
        }
    }
    Some((by, limit.clamp(1, 1000)))
}
