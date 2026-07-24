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

/// `IDX.COUNT name RANGE min max | EQ v | WHERE …` — and NOTHING
/// else: the count covers the driving range only (WHERE IS the
/// driving range), so a clause it would not apply is refused up front
/// (the server's exact order: arity before catalog).
pub(super) fn cmd_idx_count(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let Some(name) = argv.get(1) else {
        return badargs(out, "IDX.COUNT", b"");
    };
    let mut where_clause = None;
    if argv.get(2).is_some_and(|s| s.eq_ignore_ascii_case(b"WHERE")) {
        match kevy_index::parse_where(argv, 3, is_scalar_kw) {
            Some((w, next)) if next == argv.len() => where_clause = Some(w),
            _ => return badargs(out, "IDX.COUNT", name),
        }
    } else {
        let arity_ok = argv.get(2).is_some_and(|shape| {
            (shape.eq_ignore_ascii_case(b"RANGE") && argv.len() == 5)
                || (shape.eq_ignore_ascii_case(b"EQ") && argv.len() == 4)
        });
        if !arity_ok {
            return badargs(out, "IDX.COUNT", name);
        }
    }
    let Some((min, max)) = driving_bounds(s, &where_clause, argv, "IDX.COUNT", name, out)
    else {
        return;
    };
    match s.idx_count(name, &min, &max) {
        Ok(n) => int(out, n as i64),
        Err(e) => idx_err(out, name, &e),
    }
}
