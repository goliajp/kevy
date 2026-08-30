//! `IDX.QUERY` shapes (RANGE / EQ / MATCH / KNN / GROUP / GROUPS /
//! COMPOSE / HYBRID) and `IDX.COUNT`, matching the server's origin
//! reduce shapes over the embedded typed index ops.

use crate::KevyError;
use crate::store::Store;

use kevy_index::{IndexValue, ValType};

use super::idx::{badargs, decode_cursor, encode_cursor, no_such_index, value_repr};
// spec_of's only use here is the KNN handler, which is vector-gated.
#[cfg(feature = "vector")]
use super::idx::spec_of;
use super::util::{arr, bulk, err, kevy_err, nil};

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
    // Bounds speak the `@` time expressions on i64 (the server RESP
    // face's exact grammar); `now` is read once per parse so both
    // ends of a RANGE agree on it.
    let now = (kevy_store::now_unix_ms() / 1000) as i64;
    if shape.eq_ignore_ascii_case(b"RANGE") {
        let min = kevy_index::parse_literal_bound(ty, argv.get(at)?, now)?;
        let max = kevy_index::parse_literal_bound(ty, argv.get(at + 1)?, now)?;
        Some((min, max, at + 2))
    } else if shape.eq_ignore_ascii_case(b"EQ") {
        let v = kevy_index::parse_literal_bound(ty, argv.get(at)?, now)?;
        Some((v.clone(), v, at + 1))
    } else {
        None
    }
}

/// `[LIMIT n] [CURSOR c] [FIELDS f…] [HIGHLIGHT h…]` tail (server
/// `Query::parse`). `highlight` is `None` unless the verb allows it and
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

#[path = "idx_query_tail.rs"]
mod tail;
use tail::parse_tail;

#[path = "idx_query_claused.rs"]
mod claused;
use claused::claused_query;

#[path = "idx_query_where.rs"]
mod where_head;
use where_head::{cmd_idx_count, driving_bounds, parse_scalar_head};

fn cmd_idx_query(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    // The server's arity for IDX.QUERY is -4 (a minimum of four), and it
    // refuses a shorter call with Redis's wording before it looks at the
    // catalog. Matched here so the two surfaces answer the same typo the
    // same way; COMPOSE and HYBRID are longer forms and clear this bar.
    if kevy_resp::verb_arity::arity_ok("IDX.QUERY", argv.len()) == Some(false)
        && !argv.get(1).is_some_and(|n| {
            n.eq_ignore_ascii_case(b"COMPOSE") || n.eq_ignore_ascii_case(b"HYBRID")
        })
    {
        return super::idx::arity_err(out, "IDX.QUERY");
    }
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

/// `IDX.QUERY name RANGE min max | EQ v | WHERE col EQ v … [LIMIT n]
/// [CURSOR c] [FILTER …]… [SORT f ASC|DESC] [DISTINCT f] [FACET f…]
/// [OFFSET n] [FIELDS f…]`.
fn scalar_query(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let name = &argv[1];
    // Structure first (arg presence — needs no spec), then the pure
    // CURSOR × selection grammar refusal, then the catalog, then bound
    // coercion — the server's exact error ORDER, so the oracle can pin
    // any of these cases byte-for-byte.
    let Some((where_clause, tail_at)) = parse_scalar_head(argv) else {
        return badargs(out, "IDX.QUERY", name);
    };
    let Some(tail) = parse_tail(argv, tail_at, 100, 10_000, tail::TailMode::Scalar) else {
        return badargs(out, "IDX.QUERY", name);
    };
    // A selection clause re-shapes the page, so a resume point in the
    // driving order has nothing to resume — the server's exact wording.
    let selects = tail.sort.is_some()
        || tail.distinct.is_some()
        || !tail.facets.is_empty()
        || tail.offset > 0;
    if tail.cursor_raw.is_some() && selects {
        let n = String::from_utf8_lossy(name);
        return err(
            out,
            &format!("ERR IDX.QUERY '{n}': CURSOR cannot combine with SORT|DISTINCT|FACET|OFFSET"),
        );
    }
    let Some((min, max)) = driving_bounds(s, &where_clause, argv, "IDX.QUERY", name, out) else {
        return;
    };
    let cursor = match tail.cursor_raw.as_deref() {
        None | Some(b"0") => None,
        Some(raw) => match decode_cursor(raw) {
            Some((v, k)) => Some(kevy_index::Cursor { value: v, key: k }),
            None => return badargs(out, "IDX.QUERY", name),
        },
    };
    if selects || !tail.filters.is_empty() {
        return claused_query(s, name, &min, &max, cursor.as_ref(), &tail, out);
    }
    match s.idx_query(name, &min, &max, cursor.as_ref(), tail.limit) {
        Err(e) => idx_err(out, name, &e),
        Ok((rows, next)) => emit_scalar_page(s, out, &rows, next.as_ref(), &tail.fields),
    }
}

/// The plain `[cursor, rows]` scalar envelope (flat key/value pairs
/// without FIELDS, hydrated row arrays with).
fn emit_scalar_page(
    s: &Store,
    out: &mut Vec<u8>,
    rows: &[(Vec<u8>, IndexValue)],
    next: Option<&kevy_index::Cursor>,
    fields: &[Vec<u8>],
) {
    arr(out, 2);
    match next {
        Some(c) => bulk(out, &encode_cursor(&c.value, &c.key)),
        None => bulk(out, b"0"),
    }
    if fields.is_empty() {
        // legacy flat shape: *2N of key/value
        arr(out, rows.len() * 2);
        for (k, v) in rows {
            bulk(out, k);
            bulk(out, &value_repr(v));
        }
    } else {
        arr(out, rows.len());
        for (k, v) in rows {
            emit_row(s, out, k, Some(v), fields);
        }
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
        let Some(tail) = parse_tail(argv, 4, 10, 1000, tail::TailMode::Match) else {
            return badargs(out, "IDX.QUERY", name);
        };
        let want = tail.highlight.as_deref();
        let filters: Vec<crate::ValueFilter> =
            tail.filters.iter().map(tail::FilterClause::as_value_filter).collect();
        let opts = crate::MatchOpts {
            highlight: want,
            typo: tail.typo,
            offset: tail.offset,
            scope: &tail.scope,
            filters: &filters,
            sort: tail.sort.as_ref().map(|(f, d)| (f.as_slice(), *d)),
            distinct: tail.distinct.as_deref(),
            facets: &tail.facets,
        };
        let facet_extra = usize::from(!tail.facets.is_empty());
        match s.idx_match_faceted(name, text, tail.limit, opts) {
            Err(e) => idx_err(out, name, &e),
            Ok(page) if want.is_some() => {
                emit_ranked_highlighted(s, out, &page.hits, &tail.fields, facet_extra);
                emit_facets(out, &tail.facets, &page.facets);
            }
            Ok(page) => {
                let plain: Vec<(Vec<u8>, f64)> =
                    page.hits.into_iter().map(|(k, v, _)| (k, v)).collect();
                emit_ranked(s, out, &plain, &tail.fields, 4, facet_extra);
                emit_facets(out, &tail.facets, &page.facets);
            }
        }
    }
    #[cfg(not(feature = "text"))]
    {
        let _ = (s, argv);
        no_such_index(out, name);
    }
}

/// The faceted reply's one trailing element: `[field, [value, count, …],
/// field, …]`, matching the wire's shape exactly.
#[cfg(feature = "text")]
fn emit_facets(out: &mut Vec<u8>, names: &[Vec<u8>], buckets: &[Vec<(Vec<u8>, u64)>]) {
    if names.is_empty() {
        return;
    }
    arr(out, names.len() * 2);
    for (name, field) in names.iter().zip(buckets) {
        bulk(out, name);
        arr(out, field.len() * 2);
        for (label, n) in field {
            bulk(out, label);
            bulk(out, n.to_string().as_bytes());
        }
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
    // Elements the caller will append after the rows (the faceted
    // reply's one trailing element). Counted here because the array
    // header is written before them.
    extra: usize,
) {
    arr(out, hits.len() + extra);
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
    extra: usize,
) {
    arr(out, hits.len() + extra);
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
                emit_ranked(s, out, &hits, &tail.fields, 4, 0);
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
fn parse_knn_tail(argv: &[Vec<u8>]) -> Option<(tail::Tail, usize)> {
    let mut t = tail::Tail {
        limit: 10,
        cursor_raw: None,
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
