//! Ranked-list reduces: MATCH / KNN merge and the HYBRID
//! reciprocal-rank fusion.

use kevy_resp::{encode_array_len, encode_bulk, encode_error};
use kevy_rt::ExtensionReduced;

use super::chunk::{read_highlight, read_hydration, read_kbytes, read_u32};
use crate::cmd_index_query::{HitSpans, Hydrated};

/// One shard's decoded pass-1 report: `(n_docs, total_len, [(token, df)])`.
type ShardCorpus = (u64, u64, Vec<(Vec<u8>, u32)>);

/// KNN reduce: decode `[n][(key, f64, hydration)*]` chunks, sort
/// distance-ascending, truncate to LIMIT, emit `[key, value, fields…]`.
/// MATCH takes the two-pass [`reduce_match_stats`]→[`reduce_match_score`]
/// path instead so its scores are globally comparable.
pub(super) fn reduce_ranked(argv: &[Vec<u8>], chunks: &[Vec<u8>], ascending: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let Some((limit, fields)) = crate::cmd_index_query::KnnArgs::parse(argv)
        .map(|q| (q.limit, q.fields))
        .filter(|_| ascending)
    else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return out;
    };
    merge_ranked(chunks, limit, &fields, ascending, false)
}

/// MATCH pass 2 reduce: merge the globally-scored ranked chunks
/// (score-descending). Same chunk layout as KNN; `(limit, fields)` come
/// from the MATCH.SCORE argv the pass-1 reduce built.
pub(super) fn reduce_match_score(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let Some((_, _, limit, fields, highlight)) = crate::cmd_index_query::parse_match_score(argv)
    else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return out;
    };
    merge_ranked(chunks, limit, &fields, false, highlight.is_some())
}

/// Decode `[n][(key, f64, hydration, highlight?)*]` chunks, sort,
/// truncate, emit `[key, value, fields…, highlights?]` rows. Shared by
/// KNN and MATCH pass 2; `highlight` is true only for a MATCH that asked
/// for it, and then each chunk carries a highlight block per hit and each
/// row gains a trailing `[[field, start, end, …], …]` element.
fn merge_ranked(
    chunks: &[Vec<u8>],
    limit: usize,
    fields: &[Vec<u8>],
    ascending: bool,
    highlight: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut all: Vec<(f64, Vec<u8>, Hydrated, HitSpans)> = Vec::new();
    for c in chunks {
        collect_hits(c, highlight, &mut all);
    }
    if ascending {
        all.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    } else {
        all.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    }
    all.truncate(limit);
    encode_array_len(&mut out, all.len() as i64);
    for (v, key, fv, hl) in &all {
        let base = 2 + fields.len() * 2 + usize::from(highlight);
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
        if highlight {
            encode_highlights(&mut out, hl);
        }
    }
    out
}

/// Decode one shard's `[n][(key, f64, hydration, highlight?)*]` chunk
/// into `all`; a short/corrupt chunk stops that shard's contribution.
fn collect_hits(c: &[u8], highlight: bool, all: &mut Vec<(f64, Vec<u8>, Hydrated, HitSpans)>) {
    let mut pos = 1usize;
    let Some(n) = read_u32(c, &mut pos) else { return };
    for _ in 0..n {
        let Some(key) = read_kbytes(c, &mut pos) else { break };
        let Some(sb) = c.get(pos..pos + 8) else { break };
        let v = f64::from_le_bytes(sb.try_into().expect("8 bytes"));
        pos += 8;
        let Some(fv) = read_hydration(c, &mut pos) else { break };
        let hl = if highlight {
            match read_highlight(c, &mut pos) {
                Some(h) => h,
                None => break,
            }
        } else {
            Vec::new()
        };
        all.push((v, key, fv, hl));
    }
}

/// Emit the trailing highlights element: one sub-array per field,
/// `[field_name, start, end, start, end, …]` (offsets as bulk decimals,
/// matching the row's all-bulk convention).
fn encode_highlights(out: &mut Vec<u8>, hl: &HitSpans) {
    encode_array_len(out, hl.len() as i64);
    for (name, ranges) in hl {
        encode_array_len(out, (1 + ranges.len() * 2) as i64);
        encode_bulk(out, name);
        for (s, e) in ranges {
            encode_bulk(out, s.to_string().as_bytes());
            encode_bulk(out, e.to_string().as_bytes());
        }
    }
}

/// MATCH pass 1 reduce: fold each shard's corpus counters
/// (`[ST_OK][n_docs u64][total_len u64][ntok u32][(tlen,token,df u32)*]`)
/// into one global [`kevy_text::CorpusStats`], then re-fan-out
/// `MATCH.SCORE` carrying it so every shard scores against the same
/// numbers (global BM25 — a hit's rank stops depending on its shard).
///
/// Stateless two-phase like GROUPS→AGG.FETCH: the aggregated stats ride
/// inside the follow-up argv, so the runtime holds no per-phase state.
pub(super) fn reduce_match_stats(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> ExtensionReduced {
    let mut out = Vec::new();
    let Some(m) = crate::cmd_index_query::MatchArgs::parse(argv) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return ExtensionReduced::Reply(out);
    };
    let mut q_tokens = kevy_text::tokenize(&m.text);
    q_tokens.sort();
    q_tokens.dedup();
    let (mut n_docs, mut total_len) = (0u64, 0u64);
    let mut df: std::collections::HashMap<Vec<u8>, u32> =
        q_tokens.iter().map(|t| (t.clone(), 0)).collect();
    for c in chunks {
        let Some((nd, tl, tokdf)) = decode_stats_chunk(c) else { continue };
        n_docs += nd;
        total_len += tl;
        for (tok, d) in tokdf {
            if let Some(slot) = df.get_mut(&tok) {
                *slot += d;
            }
        }
    }
    let avgdl = if n_docs > 0 { total_len as f64 / n_docs as f64 } else { 0.0 };
    let blob = encode_gstats_arg(n_docs as f64, avgdl, &df);
    let mut argv2: Vec<Vec<u8>> = vec![
        b"MATCH.SCORE".to_vec(),
        m.name,
        m.text,
        format!("LIMIT={}", m.limit).into_bytes(),
        blob,
    ];
    if !m.fields.is_empty() {
        argv2.push(b"FIELDS".to_vec());
        argv2.extend(m.fields);
    }
    // Carry HIGHLIGHT to pass 2, where the segment produces the spans;
    // an empty field list means "every field".
    if let Some(hl) = m.highlight {
        argv2.push(b"HIGHLIGHT".to_vec());
        argv2.extend(hl);
    }
    ExtensionReduced::Continue(argv2)
}

/// Encode the aggregated global stats as one MATCH.SCORE argv element
/// (the per-shard decoder is `cmd_index_query::wire::decode_gstats_arg`).
/// Layout: `[n_docs f64][avgdl f64][ntok u32][(tlen u32, token, df u32)*]`.
fn encode_gstats_arg(
    n_docs: f64,
    avgdl: f64,
    df: &std::collections::HashMap<Vec<u8>, u32>,
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&n_docs.to_le_bytes());
    b.extend_from_slice(&avgdl.to_le_bytes());
    b.extend_from_slice(&(df.len() as u32).to_le_bytes());
    for (tok, d) in df {
        b.extend_from_slice(&(tok.len() as u32).to_le_bytes());
        b.extend_from_slice(tok);
        b.extend_from_slice(&d.to_le_bytes());
    }
    b
}

/// Decode one pass-1 stats chunk into `(n_docs, total_len, [(token, df)])`.
/// `None` on a status byte / truncated body.
fn decode_stats_chunk(c: &[u8]) -> Option<ShardCorpus> {
    let n_docs = u64::from_le_bytes(c.get(1..9)?.try_into().ok()?);
    let total_len = u64::from_le_bytes(c.get(9..17)?.try_into().ok()?);
    let ntok = u32::from_le_bytes(c.get(17..21)?.try_into().ok()?) as usize;
    let mut pos = 21usize;
    let mut tokdf = Vec::with_capacity(ntok);
    for _ in 0..ntok {
        let tlen = u32::from_le_bytes(c.get(pos..pos + 4)?.try_into().ok()?) as usize;
        pos += 4;
        let tok = c.get(pos..pos + tlen)?.to_vec();
        pos += tlen;
        let d = u32::from_le_bytes(c.get(pos..pos + 4)?.try_into().ok()?);
        pos += 4;
        tokdf.push((tok, d));
    }
    Some((n_docs, total_len, tokdf))
}

/// Decode one ranked segment `[n][(key, f64, hydration)*]`.
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

/// RRF fusion at the origin: globally rank the merged BM25
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
