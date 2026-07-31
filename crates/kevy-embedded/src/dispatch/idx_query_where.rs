//! The scalar driving-shape head (`RANGE` / `EQ` / composite `WHERE`)
//! plus `IDX.COUNT` — split from `idx_query.rs` for the 500-LOC house
//! rule (a `#[path]` child, sharing its parent's imports path).

use kevy_index::{IndexValue, WhereClause};

use super::super::util::{err, int};
use super::super::idx::{badargs, no_such_index, spec_of};
use super::{idx_err, parse_bounds};
use crate::store::Store;

/// The scalar clause keywords (the server's `is_scalar_keyword` set —
/// what a `WHERE` block collects up to).
pub(super) fn is_scalar_kw(a: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b"LIMIT")
        || a.eq_ignore_ascii_case(b"CURSOR")
        || a.eq_ignore_ascii_case(b"FIELDS")
        || a.eq_ignore_ascii_case(b"FILTER")
        || a.eq_ignore_ascii_case(b"SORT")
        || a.eq_ignore_ascii_case(b"DISTINCT")
        || a.eq_ignore_ascii_case(b"FACET")
        || a.eq_ignore_ascii_case(b"OFFSET")
}

/// Parse the driving shape at argv[2]: `(where_clause, tail_at)`.
/// `None` = syntax error (the caller renders badargs).
pub(super) fn parse_scalar_head(argv: &[Vec<u8>]) -> Option<(Option<WhereClause>, usize)> {
    if argv[2].eq_ignore_ascii_case(b"RANGE") && argv.len() >= 5 {
        Some((None, 5))
    } else if argv[2].eq_ignore_ascii_case(b"EQ") && argv.len() >= 4 {
        Some((None, 4))
    } else if argv[2].eq_ignore_ascii_case(b"WHERE") {
        let (w, next) = kevy_index::parse_where(argv, 3, is_scalar_kw)?;
        Some((Some(w), next))
    } else {
        None
    }
}

/// The driving bounds for a parsed head: RANGE/EQ coerce to the
/// declared type; WHERE computes the composite byte-range. `None` =
/// an error was already written (the server's exact wording — WHERE
/// errors render as the ST_CLAUSE line, bad literals as badargs).
pub(super) fn driving_bounds(
    s: &Store,
    where_clause: &Option<WhereClause>,
    argv: &[Vec<u8>],
    verb: &str,
    name: &[u8],
    out: &mut Vec<u8>,
) -> Option<(IndexValue, IndexValue)> {
    let Some(spec) = spec_of(s, name) else {
        no_such_index(out, name);
        return None;
    };
    if let Some(w) = where_clause {
        let n = String::from_utf8_lossy(name);
        let Some(cols) = &spec.composite else {
            err(out, &format!("ERR {verb} '{n}': {}", kevy_index::WHERE_NOT_COMPOSITE));
            return None;
        };
        return match kevy_index::composite_bounds(cols, w) {
            Ok((lo, hi)) => Some((IndexValue::Str(lo), IndexValue::Str(hi))),
            Err(e) => {
                err(out, &format!("ERR {verb} '{n}': {e}"));
                None
            }
        };
    }
    match parse_bounds(spec.ty, &argv[2], argv, 3) {
        Some((min, max, _)) => Some((min, max)),
        None => {
            badargs(out, verb, name);
            None
        }
    }
}

/// `IDX.COUNT name RANGE min max | EQ v | WHERE … [FILTER …]…` —
/// FILTER is applied (the claused count: the total a claused query's
/// pages would reach, materializing nothing); every clause the count
/// would NOT apply (SORT/DISTINCT/FACET/OFFSET/FIELDS/CURSOR) is
/// refused up front, the server's exact order: arity before catalog.
pub(super) fn cmd_idx_count(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let Some(name) = argv.get(1) else {
        return badargs(out, "IDX.COUNT", b"");
    };
    let Some((where_clause, tail)) = parse_count_shape(argv) else {
        return badargs(out, "IDX.COUNT", name);
    };
    let Some((min, max)) = driving_bounds(s, &where_clause, argv, "IDX.COUNT", name, out)
    else {
        return;
    };
    if tail.filters.is_empty() {
        return match s.idx_count(name, &min, &max) {
            Ok(n) => int(out, n as i64),
            Err(e) => idx_err(out, name, &e),
        };
    }
    let filters: Vec<crate::ValueFilter> =
        tail.filters.iter().map(super::tail::FilterClause::as_value_filter).collect();
    match s.idx_count_claused(name, &min, &max, &filters) {
        Ok(n) => int(out, n as i64),
        Err(crate::KevyError::InvalidInput(m)) => {
            // A clause this index cannot answer — the server frames it
            // as `ERR <verb> '<name>': <explanation>`.
            let n = String::from_utf8_lossy(name);
            err(out, &format!("ERR IDX.COUNT '{n}': {m}"));
        }
        Err(e) => idx_err(out, name, &e),
    }
}

/// COUNT's shape head + FILTER-only tail: `None` = a refusal (bad
/// shape, or a clause the count would not apply).
fn parse_count_shape(
    argv: &[Vec<u8>],
) -> Option<(Option<kevy_index::WhereClause>, super::tail::Tail)> {
    let mut where_clause = None;
    let tail_at = if argv.get(2).is_some_and(|s| s.eq_ignore_ascii_case(b"WHERE")) {
        let (w, next) = kevy_index::parse_where(argv, 3, is_scalar_kw)?;
        where_clause = Some(w);
        next
    } else if argv.get(2).is_some_and(|s| s.eq_ignore_ascii_case(b"RANGE")) && argv.len() >= 5 {
        5
    } else if argv.get(2).is_some_and(|s| s.eq_ignore_ascii_case(b"EQ")) && argv.len() >= 4 {
        4
    } else {
        return None;
    };
    let tail = super::tail::parse_tail(argv, tail_at, 100, 10_000, super::tail::TailMode::Scalar)?;
    let count_only = tail.sort.is_none()
        && tail.distinct.is_none()
        && tail.facets.is_empty()
        && tail.offset == 0
        && tail.fields.is_empty()
        && tail.cursor_raw.is_none();
    count_only.then_some((where_clause, tail))
}
