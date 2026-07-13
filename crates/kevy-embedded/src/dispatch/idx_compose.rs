//! `IDX.QUERY COMPOSE` (key-ordered two-index algebra) and
//! `IDX.QUERY HYBRID` (BM25 + KNN reciprocal-rank fusion) — split from
//! `idx_query.rs` under the 500-LOC house rule.

use crate::store::Store;

use kevy_index::IndexValue;

use super::idx::{badargs, no_such_index, spec_of, unhex};
use super::idx_query::{emit_row, idx_err, parse_bounds};
use super::util::{arr, bulk, err};

/// `IDX.QUERY COMPOSE AND|OR a <shape> b <shape> [LIMIT n] [CURSOR k]
/// [FIELDS f…]` — key-ordered set algebra over two indexes; the cursor
/// is a plain hex key point.
pub(super) fn compose(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let Some(cq) = parse_compose(s, argv) else {
        return err(out, "ERR bad IDX arguments");
    };
    let (and, a, b, limit, cursor_key, fields) = cq;
    let a_hits = match s.idx_query(&a.0, &a.1, &a.2, None, 100_000) {
        Ok((rows, _)) => rows,
        Err(e) => return idx_err(out, &a.0, &e),
    };
    let mut keys: Vec<Vec<u8>> = if and {
        let b_set: std::collections::BTreeSet<Vec<u8>> =
            match s.idx_query(&b.0, &b.1, &b.2, None, 100_000) {
                Ok((rows, _)) => rows.into_iter().map(|(k, _)| k).collect(),
                Err(e) => return idx_err(out, &b.0, &e),
            };
        a_hits.into_iter().map(|(k, _)| k).filter(|k| b_set.contains(k)).collect()
    } else {
        let mut all: Vec<Vec<u8>> = a_hits.into_iter().map(|(k, _)| k).collect();
        match s.idx_query(&b.0, &b.1, &b.2, None, 100_000) {
            Ok((rows, _)) => all.extend(rows.into_iter().map(|(k, _)| k)),
            Err(e) => return idx_err(out, &b.0, &e),
        }
        all.sort();
        all.dedup();
        all
    };
    keys.sort();
    if let Some(cur) = &cursor_key {
        keys.retain(|k| k.as_slice() > cur.as_slice());
    }
    keys.truncate(limit);
    let next = if keys.len() == limit {
        keys.last().map(|k| super::idx::hex(k)).unwrap_or_else(|| b"0".to_vec())
    } else {
        b"0".to_vec()
    };
    arr(out, 2);
    bulk(out, &next);
    arr(out, keys.len());
    for k in &keys {
        emit_row(s, out, k, None, &fields);
    }
}

/// One COMPOSE side: `(name, min, max)`.
type Sub = (Vec<u8>, IndexValue, IndexValue);
type ComposeParsed = (bool, Sub, Sub, usize, Option<Vec<u8>>, Vec<Vec<u8>>);

fn parse_compose(s: &Store, argv: &[Vec<u8>]) -> Option<ComposeParsed> {
    let and = if argv.get(2)?.eq_ignore_ascii_case(b"AND") {
        true
    } else if argv.get(2)?.eq_ignore_ascii_case(b"OR") {
        false
    } else {
        return None;
    };
    let (a, i) = parse_sub(s, argv, 3)?;
    let (b, i) = parse_sub(s, argv, i)?;
    let mut limit = 100usize;
    let mut cursor_key = None;
    let mut fields = Vec::new();
    let mut i = i;
    while i < argv.len() {
        let t = &argv[i];
        if t.eq_ignore_ascii_case(b"LIMIT") {
            limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            i += 2;
        } else if t.eq_ignore_ascii_case(b"CURSOR") {
            let raw = argv.get(i + 1)?;
            cursor_key = if raw.as_slice() == b"0" { None } else { Some(unhex(raw)?) };
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
    Some((and, a, b, limit.clamp(1, 10_000), cursor_key, fields))
}

fn parse_sub(s: &Store, argv: &[Vec<u8>], i: usize) -> Option<(Sub, usize)> {
    let name = argv.get(i)?.clone();
    let ty = spec_of(s, &name)?.ty;
    let (min, max, next) = parse_bounds(ty, argv.get(i + 1)?, argv, i + 2)?;
    Some(((name, min, max), next))
}

/// `IDX.QUERY HYBRID t MATCH q a KNN v [LIMIT n] [RRFK k] [EF e]
/// [FIELDS f…]` — reciprocal-rank fusion of the two ranked lists.
pub(super) fn hybrid(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    #[cfg(all(feature = "text", feature = "vector"))]
    {
        hybrid_impl(s, argv, out);
    }
    #[cfg(not(all(feature = "text", feature = "vector")))]
    {
        let _ = s;
        badargs(out, "IDX.QUERY", argv.get(2).map(Vec::as_slice).unwrap_or(b""));
    }
}

#[cfg(all(feature = "text", feature = "vector"))]
fn hybrid_impl(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    use std::collections::HashMap;
    let Some(q) = parse_hybrid(argv) else {
        return err(out, "ERR bad IDX arguments");
    };
    let depth = q.limit * 4;
    let matches = match s.idx_match(&q.text_idx, &q.text, depth) {
        Ok(m) => m,
        Err(e) => return idx_err(out, &q.text_idx, &e),
    };
    let Some(spec) = spec_of(s, &q.ann_idx) else {
        return no_such_index(out, &q.ann_idx);
    };
    let dim = spec.ann.map_or(0, |a| a.dim) as usize;
    let Some(vec) = kevy_vector::parse_vector(&q.vec, dim) else {
        return badargs(out, "IDX.QUERY", &q.ann_idx);
    };
    let knns = match s.idx_knn(&q.ann_idx, &vec, depth, q.ef) {
        Ok(k) => k,
        Err(e) => return idx_err(out, &q.ann_idx, &e),
    };
    // idx_match is score-descending, idx_knn distance-ascending — both
    // already globally merged, so ranks are direct.
    let mut fused: HashMap<Vec<u8>, f64> = HashMap::new();
    for (rank, (key, _)) in matches.into_iter().enumerate() {
        *fused.entry(key).or_insert(0.0) += 1.0 / (q.rrf_k + rank as f64 + 1.0);
    }
    for (rank, (key, _)) in knns.into_iter().enumerate() {
        *fused.entry(key).or_insert(0.0) += 1.0 / (q.rrf_k + rank as f64 + 1.0);
    }
    let mut all: Vec<(Vec<u8>, f64)> = fused.into_iter().collect();
    all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    all.truncate(q.limit);
    super::idx_query::emit_ranked(s, out, &all, &q.fields, 6);
}

#[cfg(all(feature = "text", feature = "vector"))]
struct HybridQ {
    text_idx: Vec<u8>,
    text: Vec<u8>,
    ann_idx: Vec<u8>,
    vec: Vec<u8>,
    limit: usize,
    rrf_k: f64,
    ef: usize,
    fields: Vec<Vec<u8>>,
}

#[cfg(all(feature = "text", feature = "vector"))]
fn parse_hybrid(argv: &[Vec<u8>]) -> Option<HybridQ> {
    if !argv.get(3)?.eq_ignore_ascii_case(b"MATCH") || !argv.get(6)?.eq_ignore_ascii_case(b"KNN") {
        return None;
    }
    let mut q = HybridQ {
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
            q.limit = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            if !(1..=1000).contains(&q.limit) {
                return None;
            }
            i += 2;
        } else if t.eq_ignore_ascii_case(b"RRFK") {
            q.rrf_k = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            if !q.rrf_k.is_finite() || q.rrf_k <= 0.0 {
                return None;
            }
            i += 2;
        } else if t.eq_ignore_ascii_case(b"EF") {
            q.ef = std::str::from_utf8(argv.get(i + 1)?).ok()?.parse().ok()?;
            if !(16..=4096).contains(&q.ef) {
                return None;
            }
            i += 2;
        } else if t.eq_ignore_ascii_case(b"FIELDS") {
            q.fields = argv[i + 1..].to_vec();
            if q.fields.is_empty() {
                return None;
            }
            break;
        } else {
            return None;
        }
    }
    Some(q)
}
