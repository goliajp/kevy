//! IDX.* argv grammar: the query shapes and their parsers. The scalar
//! shapes (`Query` / `ComposeQuery`) live in the `args_scalar.rs` child;
//! the stored-value clauses in `args_filter.rs`.

pub(crate) struct MatchArgs {
    pub(crate) name: Vec<u8>,
    pub(crate) text: Vec<u8>,
    pub(crate) limit: usize,
    pub(crate) fields: Vec<Vec<u8>>,
    /// `HIGHLIGHT [field…]`: `None` = not requested, `Some(empty)` =
    /// highlight every indexed field, `Some(fields)` = only those.
    pub(crate) highlight: Option<Vec<Vec<u8>>>,
    /// `TYPO n`: edit-distance budget for each bare term; 0 = exact.
    pub(crate) typo: u32,
    /// `OFFSET n`: hits to skip before `LIMIT` takes effect.
    pub(crate) offset: usize,
    /// `IN <field…>`: the declared fields the query is restricted to;
    /// empty = every field. Names, not positions — the mapping needs the
    /// index spec, which only the shard holding the segment has.
    pub(crate) scope: Vec<Vec<u8>>,
    /// `FILTER …`: non-scoring predicates over stored values, ANDed.
    pub(crate) filters: Vec<FilterArg>,
    /// `SORT <field> ASC|DESC`: select by a stored value instead of by
    /// score. `None` = rank by score.
    pub(crate) sort: Option<(Vec<u8>, bool)>,
    /// `DISTINCT <field>`: at most one hit per value of a stored field.
    pub(crate) distinct: Option<Vec<u8>>,
    /// `FACET <field…>`: count each field's values over the whole match
    /// set, reported alongside the page.
    pub(crate) facets: Vec<Vec<u8>>,
}

/// A MATCH clause keyword — the boundary a variadic clause (`FIELDS`,
/// `HIGHLIGHT`) collects up to.
fn is_clause_keyword(a: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b"LIMIT")
        || a.eq_ignore_ascii_case(b"FIELDS")
        || a.eq_ignore_ascii_case(b"HIGHLIGHT")
        || a.eq_ignore_ascii_case(b"TYPO")
        || a.eq_ignore_ascii_case(b"OFFSET")
        || a.eq_ignore_ascii_case(b"IN")
        || a.eq_ignore_ascii_case(b"FILTER")
        || a.eq_ignore_ascii_case(b"SORT")
        || a.eq_ignore_ascii_case(b"DISTINCT")
        || a.eq_ignore_ascii_case(b"FACET")
}

/// Parse a `TYPO` budget: 0, 1 or 2. `AUTO` (in the frozen surface but
/// not built) and anything else are a syntax error rather than a silently
/// clamped budget.
fn parse_typo(v: &[u8]) -> Option<u32> {
    match v {
        b"0" => Some(0),
        b"1" => Some(1),
        b"2" => Some(2),
        _ => None,
    }
}

/// Apply the MATCH clause starting at `i` to `a`; returns the index of
/// the next clause, or `None` on a syntax error.
fn apply_clause(argv: &[Vec<u8>], i: usize, a: &mut MatchArgs) -> Option<usize> {
    let kw = &argv[i];
    if kw.eq_ignore_ascii_case(b"LIMIT") {
        a.limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
        Some(i + 2)
    } else if kw.eq_ignore_ascii_case(b"FIELDS") {
        let (fs, next) = collect_clause(argv, i + 1);
        if fs.is_empty() {
            return None;
        }
        a.fields = fs;
        Some(next)
    } else if kw.eq_ignore_ascii_case(b"HIGHLIGHT") {
        let (hs, next) = collect_clause(argv, i + 1);
        a.highlight = Some(hs);
        Some(next)
    } else if kw.eq_ignore_ascii_case(b"TYPO") {
        a.typo = parse_typo(argv.get(i + 1)?)?;
        Some(i + 2)
    } else if kw.eq_ignore_ascii_case(b"OFFSET") {
        a.offset = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
        Some(i + 2)
    } else if kw.eq_ignore_ascii_case(b"FACET")
        || kw.eq_ignore_ascii_case(b"DISTINCT")
        || kw.eq_ignore_ascii_case(b"SORT")
    {
        apply_value_clause(argv, i, a)
    } else if kw.eq_ignore_ascii_case(b"FILTER") {
        apply_filter(argv, i, a)
    } else if kw.eq_ignore_ascii_case(b"IN") {
        let (fs, next) = collect_clause(argv, i + 1);
        if fs.is_empty() {
            return None;
        }
        a.scope = fs;
        Some(next)
    } else {
        None
    }
}

/// Collect a variadic clause's arguments from `start` until the next
/// clause keyword or the end; returns them and the next keyword index.
fn collect_clause(argv: &[Vec<u8>], start: usize) -> (Vec<Vec<u8>>, usize) {
    let mut i = start;
    let mut out = Vec::new();
    while i < argv.len() && !is_clause_keyword(&argv[i]) {
        out.push(argv[i].clone());
        i += 1;
    }
    (out, i)
}

/// Clauses of the terminal MATCH surface that parse today and execute
/// later. Listed here rather than rejected as unknown so the syntax is
/// frozen now: every one of these would otherwise want to change the
/// MATCH signature when it lands, and v4 is the release that freezes it.
const NOT_YET: &[&[u8]] = &[];

/// Outcome of parsing a MATCH query.
pub(crate) enum MatchParse {
    Ok(Box<MatchArgs>),
    /// Syntax the parser does not recognise at all.
    BadArgs,
    /// Recognised, reserved, not built yet — reported by name.
    NotYet(&'static [u8]),
}

impl MatchArgs {
    /// Parse the terminal surface, distinguishing "not valid" from "not
    /// yet". An unimplemented clause must never be silently dropped: a
    /// FILTER that is ignored returns unfiltered results, which is a
    /// wrong answer wearing a successful reply.
    pub(crate) fn parse_terminal(argv: &[Vec<u8>]) -> MatchParse {
        if let Some(i) =
            (4..argv.len()).find(|&i| NOT_YET.iter().any(|c| argv[i].eq_ignore_ascii_case(c)))
        {
            let clause =
                NOT_YET.iter().find(|c| argv[i].eq_ignore_ascii_case(c)).expect("just matched");
            return MatchParse::NotYet(clause);
        }
        match Self::parse(argv) {
            Some(a) => MatchParse::Ok(Box::new(a)),
            None => MatchParse::BadArgs,
        }
    }

    pub(crate) fn parse(argv: &[Vec<u8>]) -> Option<MatchArgs> {
        let name = argv.get(1)?.clone();
        if !argv.get(2)?.eq_ignore_ascii_case(b"MATCH") {
            return None;
        }
        let mut a = MatchArgs {
            name,
            text: argv.get(3)?.clone(),
            limit: 10,
            fields: Vec::new(),
            highlight: None,
            typo: 0,
            offset: 0,
            scope: Vec::new(),
            filters: Vec::new(),
            sort: None,
            distinct: None,
            facets: Vec::new(),
        };
        // Clauses are order-independent; each variadic one (FIELDS,
        // HIGHLIGHT) collects up to the next keyword.
        let mut i = 4;
        while i < argv.len() {
            i = apply_clause(argv, i, &mut a)?;
        }
        a.limit = a.limit.clamp(1, 1000);
        a.offset = a.offset.min(10_000);
        Some(a)
    }
}

#[path = "args_filter.rs"]
mod filter;
pub(crate) use filter::{FilterArg, FilterShape};
use filter::{apply_filter, apply_value_clause};

#[path = "args_scalar.rs"]
mod scalar;
pub(crate) use scalar::{ComposeQuery, Query, Shape};

#[path = "args_match_score.rs"]
mod match_score;
pub(crate) use match_score::parse_match_score;

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

#[cfg(test)]
#[path = "args_tests.rs"]
mod terminal_surface_tests;
