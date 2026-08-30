//! The scalar IDX.QUERY / IDX.COUNT / IDX.VERIFY / COMPOSE grammar,
//! split from `args.rs` for the 500-LOC house rule (a `#[path]` child of
//! `args`, re-exported from it).

use kevy_index::{Cursor, IndexValue, ValType};

use super::super::wire::{decode_cursor, unhex};
use super::filter::{FilterArg, parse_filter, parse_sort_dir};

pub(crate) enum Shape {
    Range {
        min: Vec<u8>,
        max: Vec<u8>,
    },
    Eq {
        value: Vec<u8>,
    },
    /// `WHERE col EQ v [col EQ v…] [RANGE col min max]` — composite
    /// indexes only. The bounds compute against the spec's declared
    /// columns (pure encoding, not planning: the caller still names
    /// the index explicitly), then behave exactly like RANGE.
    Where(kevy_index::WhereClause),
    Verify,
}

/// One side of a COMPOSE (name + its shape).
pub(crate) struct SubQuery {
    pub(in crate::cmd_index_query) name: Vec<u8>,
    pub(in crate::cmd_index_query) shape: Shape,
}

pub(crate) struct Query {
    pub(crate) name: Vec<u8>,
    pub(crate) shape: Shape,
    pub(crate) limit: usize,
    pub(crate) cursor_raw: Option<Vec<u8>>,
    /// `FIELDS f…` hydration list (owning-shard hash reads ride the
    /// chunk; empty = keys/values only).
    pub(crate) fields: Vec<Vec<u8>>,
    /// `FILTER …`: non-scoring predicates over stored values, ANDed.
    pub(crate) filters: Vec<FilterArg>,
    /// `SORT <field> ASC|DESC`: order the page by a stored value
    /// instead of the driving `(value, key)` order.
    pub(crate) sort: Option<(Vec<u8>, bool)>,
    /// `DISTINCT <field>`: at most one hit per value of a stored field.
    pub(crate) distinct: Option<Vec<u8>>,
    /// `FACET <field…>`: count each field's values over the whole match
    /// set, reported alongside the page.
    pub(crate) facets: Vec<Vec<u8>>,
    /// `OFFSET n`: hits to skip before `LIMIT` takes effect.
    pub(crate) offset: usize,
}

/// A scalar clause keyword — the boundary the variadic `FACET` collects
/// up to (`FIELDS` stays terminal, as it always was).
fn is_scalar_keyword(a: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b"LIMIT")
        || a.eq_ignore_ascii_case(b"CURSOR")
        || a.eq_ignore_ascii_case(b"FIELDS")
        || a.eq_ignore_ascii_case(b"FILTER")
        || a.eq_ignore_ascii_case(b"SORT")
        || a.eq_ignore_ascii_case(b"DISTINCT")
        || a.eq_ignore_ascii_case(b"FACET")
        || a.eq_ignore_ascii_case(b"OFFSET")
}

impl Query {
    /// `IDX.QUERY name RANGE min max | EQ v [LIMIT n] [CURSOR c]
    /// [FILTER f RANGE a b | EQ v]… [SORT f ASC|DESC] [DISTINCT f]
    /// [FACET f…] [OFFSET n] [FIELDS f…]`
    /// `IDX.COUNT name RANGE min max` / `EQ v` / `IDX.VERIFY name`
    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<Query> {
        let verb = argv.first()?;
        if verb.eq_ignore_ascii_case(b"IDX.VERIFY") {
            return Some(Query { shape: Shape::Verify, ..Query::bare(argv.get(1)?.clone()) });
        }
        let name = argv.get(1)?.clone();
        let mode = argv.get(2)?;
        let (shape, i) = if mode.eq_ignore_ascii_case(b"RANGE") {
            (Shape::Range { min: argv.get(3)?.clone(), max: argv.get(4)?.clone() }, 5)
        } else if mode.eq_ignore_ascii_case(b"EQ") {
            (Shape::Eq { value: argv.get(3)?.clone() }, 4)
        } else if mode.eq_ignore_ascii_case(b"WHERE") {
            let (w, next) = kevy_index::parse_where(argv, 3, is_scalar_keyword)?;
            (Shape::Where(w), next)
        } else {
            return None;
        };
        let mut q = Query { shape, ..Query::bare(name) };
        q.parse_tail(argv, i)?;
        q.limit = q.limit.clamp(1, 10_000);
        q.offset = q.offset.min(10_000);
        Some(q)
    }

    /// The zero-clause query for `name` (every parse starts here).
    fn bare(name: Vec<u8>) -> Query {
        Query {
            name,
            shape: Shape::Verify,
            limit: 100,
            cursor_raw: None,
            fields: Vec::new(),
            filters: Vec::new(),
            sort: None,
            distinct: None,
            facets: Vec::new(),
            offset: 0,
        }
    }

    /// Apply the clause tail from `i`; `None` on a syntax error.
    fn parse_tail(&mut self, argv: &[Vec<u8>], mut i: usize) -> Option<()> {
        while i < argv.len() {
            let a = &argv[i];
            if a.eq_ignore_ascii_case(b"LIMIT") {
                self.limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if a.eq_ignore_ascii_case(b"CURSOR") {
                self.cursor_raw = Some(argv.get(i + 1)?.clone());
                i += 2;
            } else if a.eq_ignore_ascii_case(b"OFFSET") {
                self.offset = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
                i += 2;
            } else if a.eq_ignore_ascii_case(b"FILTER") {
                let (f, next) = parse_filter(argv, i)?;
                self.filters.push(f);
                i = next;
            } else if a.eq_ignore_ascii_case(b"SORT") {
                self.sort = Some((argv.get(i + 1)?.clone(), parse_sort_dir(argv.get(i + 2)?)?));
                i += 3;
            } else if a.eq_ignore_ascii_case(b"DISTINCT") {
                self.distinct = Some(argv.get(i + 1)?.clone());
                i += 2;
            } else if a.eq_ignore_ascii_case(b"FACET") {
                let mut j = i + 1;
                while j < argv.len() && !is_scalar_keyword(&argv[j]) {
                    self.facets.push(argv[j].clone());
                    j += 1;
                }
                if self.facets.is_empty() {
                    return None;
                }
                i = j;
            } else if a.eq_ignore_ascii_case(b"FIELDS") {
                self.fields = argv[i + 1..].to_vec();
                if self.fields.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        Some(())
    }

    /// Whether any clause reshapes the selection — the set that refuses
    /// a cursor (`FILTER` alone thins the driving order and pages fine).
    pub(crate) fn selects(&self) -> bool {
        self.sort.is_some() || self.distinct.is_some() || !self.facets.is_empty() || self.offset > 0
    }

    /// Whether any clause is present at all (IDX.COUNT takes none).
    pub(crate) fn has_clauses(&self) -> bool {
        self.selects() || !self.filters.is_empty()
    }

    pub(in crate::cmd_index_query) fn bounds(
        &self,
        ty: ValType,
        now: i64,
    ) -> Option<(IndexValue, IndexValue)> {
        match &self.shape {
            Shape::Range { min, max } => Some((
                kevy_index::parse_literal_bound(ty, min, now)?,
                kevy_index::parse_literal_bound(ty, max, now)?,
            )),
            Shape::Eq { value } => {
                let v = kevy_index::parse_literal_bound(ty, value, now)?;
                Some((v.clone(), v))
            }
            Shape::Where(_) | Shape::Verify => None,
        }
    }

    /// The driving bounds against a concrete spec: RANGE/EQ coerce to
    /// the declared type; WHERE computes the composite byte-range
    /// server-side ([`kevy_index::composite_bounds`]). `Err` carries a
    /// ready-made status chunk — WHERE errors are ST_CLAUSE (the shard
    /// holding the spec knows what IS declared), bad literals stay
    /// ST_BADARGS.
    pub(in crate::cmd_index_query) fn bounds_for(
        &self,
        spec: &kevy_index::IndexSpec,
        now: i64,
    ) -> Result<(IndexValue, IndexValue), Vec<u8>> {
        if let Shape::Where(w) = &self.shape {
            let Some(cols) = &spec.composite else {
                return Err(crate::cmd_index_query::query_claused::clause_chunk(
                    kevy_index::WHERE_NOT_COMPOSITE,
                ));
            };
            let (lo, hi) = kevy_index::composite_bounds(cols, w, now)
                .map_err(|e| crate::cmd_index_query::query_claused::clause_chunk(&e))?;
            return Ok((IndexValue::Str(lo), IndexValue::Str(hi)));
        }
        self.bounds(spec.ty, now).ok_or_else(|| vec![crate::cmd_index_query::ST_BADARGS])
    }

    pub(in crate::cmd_index_query) fn cursor(&self, _ty: ValType) -> Option<Cursor> {
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
                shape: Shape::Range {
                    min: argv.get(i + 2)?.clone(),
                    max: argv.get(i + 3)?.clone(),
                },
            },
            i + 4,
        ))
    } else if mode.eq_ignore_ascii_case(b"EQ") {
        Some((SubQuery { name, shape: Shape::Eq { value: argv.get(i + 2)?.clone() } }, i + 3))
    } else {
        None
    }
}
