//! `IDX.QUERY` shapes (RANGE / EQ / MATCH / KNN / GROUP / GROUPS /
//! COMPOSE / HYBRID) and `IDX.COUNT`, matching the server's origin
//! reduce shapes over the embedded typed index ops.

use crate::store::Store;
use crate::KevyError;

use kevy_index::{IndexValue, ValType};

use super::idx::{badargs, decode_cursor, encode_cursor, no_such_index, spec_of, value_repr};
use super::util::{arr, bulk, err, int, kevy_err, nil};

/// One IDX query request; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"IDX.QUERY" => cmd_idx_query(s, argv, out),
        b"IDX.COUNT" => cmd_idx_count(s, argv, out),
        _ => return false,
    }
    true
}

/// A `NotFound` from the typed ops renders as the server's
/// self-explaining no-such-index error; everything else maps as usual.
pub(super) fn idx_err(out: &mut Vec<u8>, name: &[u8], e: &KevyError) {
    match e {
        KevyError::NotFound(_) => no_such_index(out, name),
        other => kevy_err(out, other),
    }
}

/// `min max` bounds for a RANGE / EQ shape, coerced to the index type.
pub(super) fn parse_bounds(
    ty: ValType,
    shape: &[u8],
    argv: &[Vec<u8>],
    at: usize,
) -> Option<(IndexValue, IndexValue, usize)> {
    if shape.eq_ignore_ascii_case(b"RANGE") {
        let min = IndexValue::parse_literal(ty, argv.get(at)?)?;
        let max = IndexValue::parse_literal(ty, argv.get(at + 1)?)?;
        Some((min, max, at + 2))
    } else if shape.eq_ignore_ascii_case(b"EQ") {
        let v = IndexValue::parse_literal(ty, argv.get(at)?)?;
        Some((v.clone(), v, at + 1))
    } else {
        None
    }
}

/// `[LIMIT n] [CURSOR c] [FIELDS f…] [HIGHLIGHT h…]` tail (server
/// `Query::parse`). `highlight` is `None` unless the verb allows it and
/// the query asked; `Some(empty)` means every field.
struct Tail {
    limit: usize,
    cursor_raw: Option<Vec<u8>>,
    fields: Vec<Vec<u8>>,
    highlight: Option<Vec<Vec<u8>>>,
    /// `TYPO n`: edit budget for each bare term; 0 = exact.
    typo: u32,
    /// `OFFSET n`: hits to skip before `LIMIT`.
    offset: usize,
}

/// A tail clause keyword — the boundary a variadic clause collects up to.
fn is_tail_keyword(a: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b"LIMIT")
        || a.eq_ignore_ascii_case(b"CURSOR")
        || a.eq_ignore_ascii_case(b"FIELDS")
        || a.eq_ignore_ascii_case(b"HIGHLIGHT")
        || a.eq_ignore_ascii_case(b"TYPO")
        || a.eq_ignore_ascii_case(b"OFFSET")
}

/// Collect a variadic clause's args from `start` until the next keyword.
fn collect_until_keyword(argv: &[Vec<u8>], start: usize) -> (Vec<Vec<u8>>, usize) {
    let mut i = start;
    let mut out = Vec::new();
    while i < argv.len() && !is_tail_keyword(&argv[i]) {
        out.push(argv[i].clone());
        i += 1;
    }
    (out, i)
}

fn parse_tail(
    argv: &[Vec<u8>],
    mut i: usize,
    default_limit: usize,
    cap: usize,
    match_clauses: bool,
) -> Option<Tail> {
    let mut t = Tail {
        limit: default_limit,
        cursor_raw: None,
        fields: Vec::new(),
        highlight: None,
        typo: 0,
        offset: 0,
    };
    while i < argv.len() {
        i = apply_tail_clause(argv, i, &mut t, match_clauses)?;
    }
    t.limit = t.limit.clamp(1, cap);
    Some(t)
}

/// Apply the tail clause starting at `i`; returns the next index, or
/// `None` on a syntax error. `match_clauses` gates the MATCH-only ones
/// (HIGHLIGHT / TYPO / OFFSET) so a RANGE query cannot smuggle them in.
fn apply_tail_clause(
    argv: &[Vec<u8>],
    i: usize,
    t: &mut Tail,
    match_clauses: bool,
) -> Option<usize> {
    let a = &argv[i];
    if a.eq_ignore_ascii_case(b"LIMIT") {
        t.limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
        Some(i + 2)
    } else if a.eq_ignore_ascii_case(b"CURSOR") {
        t.cursor_raw = Some(argv.get(i + 1)?.clone());
        Some(i + 2)
    } else if a.eq_ignore_ascii_case(b"FIELDS") {
        let (fs, next) = collect_until_keyword(argv, i + 1);
        if fs.is_empty() {
            return None;
        }
        t.fields = fs;
        Some(next)
    } else if match_clauses && a.eq_ignore_ascii_case(b"HIGHLIGHT") {
        let (hs, next) = collect_until_keyword(argv, i + 1);
        t.highlight = Some(hs);
        Some(next)
    } else if match_clauses && a.eq_ignore_ascii_case(b"TYPO") {
        t.typo = match argv.get(i + 1)?.as_slice() {
            b"0" => 0,
            b"1" => 1,
            b"2" => 2,
            _ => return None,
        };
        Some(i + 2)
    } else if match_clauses && a.eq_ignore_ascii_case(b"OFFSET") {
        t.offset = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
        Some(i + 2)
    } else {
        None
    }
}

/// One hydrated row: `*(1|2)+2F [key, value?, (fname, fval|nil)…]` —
/// fields read through the typed `hget` (in-process hydration).
pub(super) fn emit_row(
    s: &Store,
    out: &mut Vec<u8>,
    key: &[u8],
    value: Option<&IndexValue>,
    fields: &[Vec<u8>],
) {
    arr(out, 1 + usize::from(value.is_some()) + fields.len() * 2);
    bulk(out, key);
    if let Some(v) = value {
        bulk(out, &value_repr(v));
    }
    for f in fields {
        bulk(out, f);
        match s.hget(key, f) {
            Ok(Some(v)) => bulk(out, &v),
            _ => nil(out),
        }
    }
}

fn cmd_idx_query(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let Some(name) = argv.get(1) else {
        return badargs(out, "IDX.QUERY", b"");
    };
    if name.eq_ignore_ascii_case(b"COMPOSE") {
        return super::idx_compose::compose(s, argv, out);
    }
    if name.eq_ignore_ascii_case(b"HYBRID") {
        return super::idx_compose::hybrid(s, argv, out);
    }
    let Some(shape) = argv.get(2) else {
        return badargs(out, "IDX.QUERY", name);
    };
    if shape.eq_ignore_ascii_case(b"MATCH") {
        return text_match(s, argv, out);
    }
    if shape.eq_ignore_ascii_case(b"KNN") {
        return knn(s, argv, out);
    }
    if shape.eq_ignore_ascii_case(b"GROUP") || shape.eq_ignore_ascii_case(b"GROUPS") {
        return group(s, argv, out);
    }
    scalar_query(s, argv, out);
}

/// `IDX.QUERY name RANGE min max | EQ v [LIMIT n] [CURSOR c] [FIELDS f…]`.
fn scalar_query(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let name = &argv[1];
    let Some(spec) = spec_of(s, name) else {
        return no_such_index(out, name);
    };
    let parsed = parse_bounds(spec.ty, &argv[2], argv, 3)
        .and_then(|(min, max, i)| Some((min, max, parse_tail(argv, i, 100, 10_000, false)?)));
    let Some((min, max, tail)) = parsed else {
        return badargs(out, "IDX.QUERY", name);
    };
    let cursor = match tail.cursor_raw.as_deref() {
        None | Some(b"0") => None,
        Some(raw) => match decode_cursor(raw) {
            Some((v, k)) => Some(kevy_index::Cursor { value: v, key: k }),
            None => return badargs(out, "IDX.QUERY", name),
        },
    };
    match s.idx_query(name, &min, &max, cursor.as_ref(), tail.limit) {
        Err(e) => idx_err(out, name, &e),
        Ok((rows, next)) => {
            arr(out, 2);
            match next {
                Some(c) => bulk(out, &encode_cursor(&c.value, &c.key)),
                None => bulk(out, b"0"),
            }
            if tail.fields.is_empty() {
                // legacy flat shape: *2N of key/value
                arr(out, rows.len() * 2);
                for (k, v) in &rows {
                    bulk(out, k);
                    bulk(out, &value_repr(v));
                }
            } else {
                arr(out, rows.len());
                for (k, v) in &rows {
                    emit_row(s, out, k, Some(v), &tail.fields);
                }
            }
        }
    }
}

/// `IDX.COUNT name RANGE min max | EQ v`.
fn cmd_idx_count(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let Some(name) = argv.get(1) else {
        return badargs(out, "IDX.COUNT", b"");
    };
    let Some(spec) = spec_of(s, name) else {
        return no_such_index(out, name);
    };
    let bounds = argv.get(2).and_then(|shape| parse_bounds(spec.ty, shape, argv, 3));
    let Some((min, max, end)) = bounds else {
        return badargs(out, "IDX.COUNT", name);
    };
    if end != argv.len() {
        return badargs(out, "IDX.COUNT", name);
    }
    match s.idx_count(name, &min, &max) {
        Ok(n) => int(out, n as i64),
        Err(e) => idx_err(out, name, &e),
    }
}

/// `IDX.QUERY name MATCH text [LIMIT n] [FIELDS f…]` — BM25 ranked.
fn text_match(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let name = &argv[1];
    #[cfg(feature = "text")]
    {
        let Some(text) = argv.get(3) else {
            return badargs(out, "IDX.QUERY", name);
        };
        let Some(tail) = parse_tail(argv, 4, 10, 1000, true) else {
            return badargs(out, "IDX.QUERY", name);
        };
        let want = tail.highlight.as_deref();
        match s.idx_match_highlighted(name, text, tail.limit, want, tail.typo, tail.offset) {
            Err(e) => idx_err(out, name, &e),
            Ok(hits) if want.is_some() => emit_ranked_highlighted(s, out, &hits, &tail.fields),
            Ok(hits) => {
                let plain: Vec<(Vec<u8>, f64)> =
                    hits.into_iter().map(|(k, v, _)| (k, v)).collect();
                emit_ranked(s, out, &plain, &tail.fields, 4);
            }
        }
    }
    #[cfg(not(feature = "text"))]
    {
        let _ = (s, argv);
        no_such_index(out, name);
    }
}

/// Ranked rows: `*N of [key, score, fields…]` with the server's
/// `{v:.prec}` score formatting (4 for MATCH/KNN, 6 for HYBRID).
#[cfg(any(feature = "text", feature = "vector"))]
pub(super) fn emit_ranked(
    s: &Store,
    out: &mut Vec<u8>,
    hits: &[(Vec<u8>, f64)],
    fields: &[Vec<u8>],
    prec: usize,
) {
    arr(out, hits.len());
    for (key, v) in hits {
        arr(out, 2 + fields.len() * 2);
        bulk(out, key);
        bulk(out, format!("{v:.prec$}").as_bytes());
        for f in fields {
            bulk(out, f);
            match s.hget(key, f) {
                Ok(Some(val)) => bulk(out, &val),
                _ => nil(out),
            }
        }
    }
}

/// Ranked rows with a trailing highlights element, matching the server's
/// `[key, score, fields…, [[field, start, end, …], …]]` shape.
#[cfg(feature = "text")]
fn emit_ranked_highlighted(
    s: &Store,
    out: &mut Vec<u8>,
    hits: &[crate::ops_index::HighlightedHit],
    fields: &[Vec<u8>],
) {
    arr(out, hits.len());
    for (key, v, hl) in hits {
        arr(out, 2 + fields.len() * 2 + 1);
        bulk(out, key);
        bulk(out, format!("{v:.4}").as_bytes());
        for f in fields {
            bulk(out, f);
            match s.hget(key, f) {
                Ok(Some(val)) => bulk(out, &val),
                _ => nil(out),
            }
        }
        arr(out, hl.len());
        for (name, ranges) in hl {
            arr(out, 1 + ranges.len() * 2);
            bulk(out, name);
            for (start, end) in ranges {
                bulk(out, start.to_string().as_bytes());
                bulk(out, end.to_string().as_bytes());
            }
        }
    }
}

/// `IDX.QUERY name KNN vec [LIMIT k] [EF e] [FIELDS f…]`.
fn knn(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let name = &argv[1];
    #[cfg(feature = "vector")]
    {
        let Some(spec) = spec_of(s, name) else {
            return no_such_index(out, name);
        };
        let Some(raw_vec) = argv.get(3) else {
            return badargs(out, "IDX.QUERY", name);
        };
        let Some((tail, ef)) = parse_knn_tail(argv) else {
            return badargs(out, "IDX.QUERY", name);
        };
        let dim = spec.ann.map_or(0, |a| a.dim) as usize;
        let Some(vec) = kevy_vector::parse_vector(raw_vec, dim) else {
            return badargs(out, "IDX.QUERY", name);
        };
        match s.idx_knn(name, &vec, tail.limit, ef) {
            Err(e) => idx_err(out, name, &e),
            Ok(hits) => {
                let hits: Vec<(Vec<u8>, f64)> =
                    hits.into_iter().map(|(k, d)| (k, f64::from(d))).collect();
                emit_ranked(s, out, &hits, &tail.fields, 4);
            }
        }
    }
    #[cfg(not(feature = "vector"))]
    {
        let _ = (s, argv);
        no_such_index(out, name);
    }
}

/// `[LIMIT k] [EF e] [FIELDS f…]` tail for KNN (EF bounds 16..=4096).
#[cfg(feature = "vector")]
fn parse_knn_tail(argv: &[Vec<u8>]) -> Option<(Tail, usize)> {
    let mut t =
        Tail { limit: 10, cursor_raw: None, fields: Vec::new(), highlight: None, typo: 0, offset: 0 };
    let mut ef = 0usize;
    let mut i = 4;
    while i < argv.len() {
        let a = &argv[i];
        if a.eq_ignore_ascii_case(b"LIMIT") {
            t.limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            i += 2;
        } else if a.eq_ignore_ascii_case(b"EF") {
            ef = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            if !(16..=4096).contains(&ef) {
                return None;
            }
            i += 2;
        } else if a.eq_ignore_ascii_case(b"FIELDS") {
            t.fields = argv[i + 1..].to_vec();
            if t.fields.is_empty() {
                return None;
            }
            break;
        } else {
            return None;
        }
    }
    t.limit = t.limit.clamp(1, 1000);
    Some((t, ef))
}

/// `IDX.QUERY name GROUP g` → `*5 [count, sum, min, max, avg]`;
/// `IDX.QUERY name GROUPS [BY m] [LIMIT n]` → `*N of *5 rows`.
fn group(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let name = &argv[1];
    if argv[2].eq_ignore_ascii_case(b"GROUP") {
        let Some(g) = argv.get(3) else {
            return badargs(out, "IDX.QUERY", name);
        };
        return match s.idx_group(name, g) {
            Err(e) => idx_err(out, name, &e),
            Ok(st) => {
                arr(out, 5);
                emit_group_stats(out, &st);
                match st.avg() {
                    Some(a) => bulk(out, format!("{a}").as_bytes()),
                    None => nil(out),
                }
            }
        };
    }
    let Some((by, limit)) = parse_groups_args(argv) else {
        return err(out, "ERR bad IDX arguments");
    };
    match s.idx_groups(name, by, limit) {
        Err(e) => idx_err(out, name, &e),
        Ok(rows) => {
            let rows: Vec<_> = rows.into_iter().filter(|(_, st)| st.count > 0).collect();
            arr(out, rows.len());
            for (g, st) in &rows {
                arr(out, 5);
                bulk(out, g);
                emit_group_stats(out, st);
            }
        }
    }
}

/// The `count, sum, min|nil, max|nil` core of a group row.
fn emit_group_stats(out: &mut Vec<u8>, st: &kevy_index::GroupStats) {
    bulk(out, st.count.to_string().as_bytes());
    bulk(out, format!("{}", st.sum).as_bytes());
    for v in [&st.min, &st.max] {
        match v {
            Some(x) => bulk(out, &value_repr(x)),
            None => nil(out),
        }
    }
}

/// `GROUPS [BY m] [LIMIT n]` tail (server `parse_groups_args`).
fn parse_groups_args(argv: &[Vec<u8>]) -> Option<(kevy_index::AggBy, usize)> {
    let (mut by, mut limit) = (kevy_index::AggBy::Count, 100usize);
    let mut i = 3;
    while i < argv.len() {
        if argv[i].eq_ignore_ascii_case(b"BY") {
            by = kevy_index::AggBy::parse(argv.get(i + 1)?)?;
            i += 2;
        } else if argv[i].eq_ignore_ascii_case(b"LIMIT") {
            limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            i += 2;
        } else {
            return None;
        }
    }
    Some((by, limit.clamp(1, 1000)))
}
