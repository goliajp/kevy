//! Ranked-list reduces: MATCH / KNN merge and the v3.13 HYBRID
//! reciprocal-rank fusion.

use kevy_resp::{encode_array_len, encode_bulk, encode_error};

use super::chunk::{read_hydration, read_kbytes, read_u32};
use crate::cmd_index_query::Hydrated;

/// Shared MATCH/KNN reduce: decode `[n][(key, f64, hydration)*]`
/// chunks, sort (ascending for KNN distances, descending for BM25
/// scores), truncate to LIMIT, emit `[key, value, fields…]` rows.
pub(super) fn reduce_ranked(argv: &[Vec<u8>], chunks: &[Vec<u8>], ascending: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let Some((limit, fields)) = ranked_args(argv, ascending) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return out;
    };
    let mut all: Vec<(f64, Vec<u8>, Hydrated)> = Vec::new();
    for c in chunks {
        let mut pos = 1usize;
        let Some(n) = read_u32(c, &mut pos) else { continue };
        for _ in 0..n {
            let Some(key) = read_kbytes(c, &mut pos) else { break };
            let Some(sb) = c.get(pos..pos + 8) else { break };
            let v = f64::from_le_bytes(sb.try_into().expect("8 bytes"));
            pos += 8;
            let Some(fv) = read_hydration(c, &mut pos) else { break };
            all.push((v, key, fv));
        }
    }
    if ascending {
        all.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    } else {
        all.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    }
    all.truncate(limit);
    encode_array_len(&mut out, all.len() as i64);
    for (v, key, fv) in &all {
        let base = 2 + fields.len() * 2;
        encode_array_len(&mut out, base as i64);
        encode_bulk(&mut out, key);
        encode_bulk(&mut out, format!("{v:.4}").as_bytes());
        for (f, val) in fields.iter().zip(fv.iter().chain(std::iter::repeat(&None))) {
            encode_bulk(&mut out, f);
            match val {
                Some(b) => encode_bulk(&mut out, b),
                None => out.extend_from_slice(b"$-1\r\n"),
            }
        }
    }
    out
}

/// `(limit, fields)` from the KNN (ascending) or MATCH argv.
fn ranked_args(argv: &[Vec<u8>], ascending: bool) -> Option<(usize, Vec<Vec<u8>>)> {
    if ascending {
        crate::cmd_index_query::KnnArgs::parse(argv).map(|q| (q.limit, q.fields))
    } else {
        crate::cmd_index_query::MatchArgs::parse(argv).map(|q| (q.limit, q.fields))
    }
}

/// v3.13 — decode one ranked segment `[n][(key, f64, hydration)*]`.
fn read_ranked_segment(
    c: &[u8],
    pos: &mut usize,
) -> Vec<(f64, Vec<u8>, Hydrated)> {
    let mut out = Vec::new();
    let Some(n) = read_u32(c, pos) else { return out };
    for _ in 0..n {
        let Some(key) = read_kbytes(c, pos) else { break };
        let Some(sb) = c.get(*pos..*pos + 8) else { break };
        let v = f64::from_le_bytes(sb.try_into().expect("8 bytes"));
        *pos += 8;
        let Some(fv) = read_hydration(c, pos) else { break };
        out.push((v, key, fv));
    }
    out
}

/// v3.13 — RRF fusion at the origin: globally rank the merged BM25
/// list (score desc) and the merged KNN list (distance asc), then
/// score(d) = Σ 1/(rrf_k + rank_i(d)) and keep the top `limit`.
/// Rank-only fusion needs no score normalization across the two
/// heterogeneous metrics — that's why RRF and not a weighted sum.
pub(super) fn reduce_hybrid(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    use std::collections::HashMap;
    let mut out = Vec::new();
    let Some(q) = crate::cmd_index_query::HybridArgs::parse(argv) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return out;
    };
    let mut matches: Vec<(f64, Vec<u8>, Hydrated)> = Vec::new();
    let mut knns: Vec<(f64, Vec<u8>, Hydrated)> = Vec::new();
    for c in chunks {
        let mut pos = 1usize;
        matches.extend(read_ranked_segment(c, &mut pos));
        knns.extend(read_ranked_segment(c, &mut pos));
    }
    matches.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    knns.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut fused: HashMap<Vec<u8>, (f64, Hydrated)> = HashMap::new();
    for (rank, (_, key, fv)) in matches.into_iter().enumerate() {
        let s = 1.0 / (q.rrf_k + rank as f64 + 1.0);
        let e = fused.entry(key).or_insert((0.0, fv));
        e.0 += s;
    }
    for (rank, (_, key, fv)) in knns.into_iter().enumerate() {
        let s = 1.0 / (q.rrf_k + rank as f64 + 1.0);
        let e = fused.entry(key).or_insert((0.0, fv));
        e.0 += s;
    }
    let mut all: Vec<(f64, Vec<u8>, Hydrated)> =
        fused.into_iter().map(|(k, (s, fv))| (s, k, fv)).collect();
    all.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    all.truncate(q.limit);
    encode_array_len(&mut out, all.len() as i64);
    for (v, key, fv) in &all {
        let base = 2 + q.fields.len() * 2;
        encode_array_len(&mut out, base as i64);
        encode_bulk(&mut out, key);
        encode_bulk(&mut out, format!("{v:.6}").as_bytes());
        for (f, val) in q.fields.iter().zip(fv.iter().chain(std::iter::repeat(&None))) {
            encode_bulk(&mut out, f);
            match val {
                Some(v) => encode_bulk(&mut out, v),
                None => out.extend_from_slice(b"$-1\r\n"),
            }
        }
    }
    out
}
