//! Ranked-list reduces: MATCH / KNN merge and the HYBRID
//! reciprocal-rank fusion.

use kevy_resp::{encode_array_len, encode_bulk, encode_error};
use kevy_rt::ExtensionReduced;

use super::chunk::{read_highlight, read_hydration, read_kbytes, read_u32};
#[path = "ranked_chunk.rs"]
mod chunk;
use chunk::{collect_facets, collect_hits};

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
    merge_ranked(
        chunks,
        Merge {
            limit,
            fields: &fields,
            ascending,
            highlight: false,
            offset: 0,
            sort_desc: None,
            grouped: false,
            facets: &[],
        },
    )
}

/// MATCH pass 2 reduce: merge the globally-scored ranked chunks
/// (score-descending). Same chunk layout as KNN; `(limit, fields)` come
/// from the MATCH.SCORE argv the pass-1 reduce built.
pub(super) fn reduce_match_score(argv: &[Vec<u8>], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(q) = crate::cmd_index_query::parse_match_score(argv) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return out;
    };
    let sort_desc = q.sort.as_ref().map(|(_, desc)| *desc);
    merge_ranked(
        chunks,
        Merge {
            limit: q.limit,
            fields: &q.fields,
            ascending: false,
            highlight: q.highlight.is_some(),
            offset: q.offset,
            sort_desc,
            grouped: q.distinct.is_some(),
            facets: &q.facets,
        },
    )
}

/// Emit the merged page as RESP rows.
fn emit_rows(
    out: &mut Vec<u8>,
    all: &[Hit],
    fields: &[Vec<u8>],
    highlight: bool,
    facets: &[Vec<u8>],
    buckets: &[Vec<Bucket>],
) {
    // A faceted reply gains ONE trailing element; without the clause the
    // array is exactly what it was before facets existed.
    encode_array_len(out, (all.len() + usize::from(!facets.is_empty())) as i64);
    for h in all {
        let base = 2 + fields.len() * 2 + usize::from(highlight);
        encode_array_len(out, base as i64);
        encode_bulk(out, &h.key);
        encode_bulk(out, format!("{:.4}", h.score).as_bytes());
        for (f, val) in fields.iter().zip(h.fields.iter().chain(std::iter::repeat(&None))) {
            encode_bulk(out, f);
            match val {
                Some(b) => encode_bulk(out, b),
                None => out.extend_from_slice(b"$-1\r\n"),
            }
        }
        if highlight {
            encode_highlights(out, &h.spans);
        }
    }
    emit_facets(out, facets, buckets);
}

/// The faceted reply's one trailing element: `[field, [value, count, …],
/// field, …]`, each field's buckets most-frequent-first with the label
/// breaking ties so the order is stable across runs.
fn emit_facets(out: &mut Vec<u8>, facets: &[Vec<u8>], buckets: &[Vec<Bucket>]) {
    if facets.is_empty() {
        return;
    }
    encode_array_len(out, (facets.len() * 2) as i64);
    for (name, field) in facets.iter().zip(buckets) {
        encode_bulk(out, name);
        let mut sorted = field.clone();
        sorted.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
        encode_array_len(out, (sorted.len() * 2) as i64);
        for (_, label, n) in &sorted {
            encode_bulk(out, label);
            encode_bulk(out, n.to_string().as_bytes());
        }
    }
}

/// The `FILTER` predicates, in the grammar pass 2 re-parses them with.
fn push_filters(argv2: &mut Vec<Vec<u8>>, filters: Vec<crate::cmd_index_query::FilterArg>) {
    for f in filters {
        argv2.push(b"FILTER".to_vec());
        argv2.push(f.field);
        match f.shape {
            crate::cmd_index_query::FilterShape::Range { min, max } => {
                argv2.push(b"RANGE".to_vec());
                argv2.push(min);
                argv2.push(max);
            }
            crate::cmd_index_query::FilterShape::Eq { value } => {
                argv2.push(b"EQ".to_vec());
                argv2.push(value);
            }
        }
    }
}

/// Collapse the union of the shards' pages: two shards can each hold a
/// document with the same value, and only the better one survives.
///
/// `all` is already in the page's order, so the first occurrence of a
/// value is its best and a stable retain keeps exactly that. Documents
/// with no value are their own group and all survive — the same rule each
/// shard collapsed by.
fn collapse_union(all: &mut Vec<Hit>) {
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    all.retain(|h| match &h.dkey {
        Some(k) => seen.insert(k.clone()),
        None => true,
    });
}

/// One facet bucket in flight: the identity shards are summed by, a
/// label that occurs in the corpus, and the running count.
pub(super) type Bucket = (Vec<u8>, Vec<u8>, u64);

/// One merged hit.
struct Hit {
    score: f64,
    key: Vec<u8>,
    fields: Hydrated,
    spans: HitSpans,
    /// The sort field's order key, when the query sorted by one.
    okey: Option<Vec<u8>>,
    /// The distinct field's identity, when the query collapsed by one.
    dkey: Option<Vec<u8>>,
}

/// Decode `[n][(key, f64, hydration, highlight?)*]` chunks, sort,
/// truncate, emit `[key, value, fields…, highlights?]` rows. Shared by
/// KNN and MATCH pass 2; `highlight` is true only for a MATCH that asked
/// for it, and then each chunk carries a highlight block per hit and each
/// row gains a trailing `[[field, start, end, …], …]` element.
/// How a merged page is shaped: what each hit carries back and how the
/// union is ordered, cut and collapsed.
struct Merge<'a> {
    limit: usize,
    fields: &'a [Vec<u8>],
    /// Score-ascending (KNN distance) rather than descending.
    ascending: bool,
    highlight: bool,
    offset: usize,
    /// `Some(desc)` when the page is ordered by a stored value.
    sort_desc: Option<bool>,
    /// Whether the page collapses by a stored value.
    grouped: bool,
    /// The `FACET` field names, in the order they were asked for. Empty
    /// = no facets, and then the reply keeps its previous shape exactly.
    facets: &'a [Vec<u8>],
}

fn merge_ranked(chunks: &[Vec<u8>], m: Merge<'_>) -> Vec<u8> {
    let Merge { limit, fields, ascending, highlight, offset, sort_desc, grouped, facets } = m;
    let mut buckets = vec![Vec::new(); facets.len()];
    let mut out = Vec::new();
    let mut all: Vec<Hit> = Vec::new();
    for c in chunks {
        let read = collect_hits(c, highlight, sort_desc.is_some(), grouped, &mut all);
        collect_facets(c, read, facets.len(), &mut buckets);
    }
    match sort_desc {
        // Order the union exactly as each shard ordered its own page —
        // one definition of that order, in kevy-text, used by both.
        Some(desc) => all.sort_by(|a, b| {
            kevy_text::sorted_order((a.okey.as_deref(), &a.key), (b.okey.as_deref(), &b.key), desc)
        }),
        None if ascending => {
            all.sort_by(|a, b| a.score.total_cmp(&b.score).then_with(|| a.key.cmp(&b.key)));
        }
        None => all.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key))),
    }
    if grouped {
        collapse_union(&mut all);
    }
    // OFFSET applies to the MERGED ranking, not per shard: drop the
    // first `offset` globally-ranked hits, then fill LIMIT.
    if offset > 0 {
        all.drain(..offset.min(all.len()));
    }
    all.truncate(limit);
    emit_rows(&mut out, &all, fields, highlight, facets, &buckets);
    out
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
    let Some(mut m) = crate::cmd_index_query::MatchArgs::parse(argv) else {
        encode_error(&mut out, "ERR bad IDX arguments");
        return ExtensionReduced::Reply(out);
    };
    let (mut n_docs, mut total_len) = (0u64, 0u64);
    // Accumulate every token each shard reports — the query's tokens and,
    // for a `word*` prefix, that shard's expansion terms — into one global
    // df, so a prefix expansion scores against its corpus-wide df too.
    let mut df: std::collections::HashMap<Vec<u8>, u32> = std::collections::HashMap::new();
    for c in chunks {
        let Some((nd, tl, tokdf)) = decode_stats_chunk(c) else { continue };
        n_docs += nd;
        total_len += tl;
        for (tok, d) in tokdf {
            *df.entry(tok).or_insert(0) += d;
        }
    }
    let avgdl = if n_docs > 0 { total_len as f64 / n_docs as f64 } else { 0.0 };
    let blob = encode_gstats_arg(n_docs as f64, avgdl, &df);
    let mut argv2: Vec<Vec<u8>> = vec![
        b"MATCH.SCORE".to_vec(),
        std::mem::take(&mut m.name),
        std::mem::take(&mut m.text),
        format!("LIMIT={}", m.limit).into_bytes(),
        blob,
    ];
    push_clauses(&mut argv2, m);
    ExtensionReduced::Continue(argv2)
}

/// Carry the user's MATCH clauses onto the pass-2 argv.
///
/// Pass 2 re-parses these with the very parser pass 1 used, so a clause
/// missing here simply does not happen on the second pass — which is why
/// they are gathered in one place instead of inline at the call.
fn push_clauses(argv2: &mut Vec<Vec<u8>>, m: crate::cmd_index_query::MatchArgs) {
    if !m.fields.is_empty() {
        argv2.push(b"FIELDS".to_vec());
        argv2.extend(m.fields);
    }
    // HIGHLIGHT goes to pass 2, where the segment produces the spans; an
    // empty field list means "every field".
    if let Some(hl) = m.highlight {
        argv2.push(b"HIGHLIGHT".to_vec());
        argv2.extend(hl);
    }
    // The typo budget rides along too: pass 2 fuzzes each bare term
    // against its own shard's dictionary.
    if m.typo > 0 {
        argv2.push(b"TYPO".to_vec());
        argv2.push(m.typo.to_string().into_bytes());
    }
    // And the field scope: pass 2 maps the names onto positions with its
    // own shard's spec, the same mapping pass 1 already validated.
    if !m.scope.is_empty() {
        argv2.push(b"IN".to_vec());
        argv2.extend(m.scope);
    }
    if m.offset > 0 {
        argv2.push(b"OFFSET".to_vec());
        argv2.push(m.offset.to_string().into_bytes());
    }
    // FILTER travels to pass 2 and is applied only there. It is
    // non-scoring: it decides which documents are eligible, not what a
    // term is worth, so pass 1's corpus statistics stay whole-corpus and
    // a filtered search ranks by the same notion of term importance an
    // unfiltered one would. (`IN` is the other axis and does move the
    // statistics — it changes WHERE IN a document we look, which changes
    // what a frequency and a length mean.)
    push_filters(argv2, m.filters);
    push_order(argv2, m.sort, m.distinct);
    push_facets(argv2, m.facets);
}

/// The clauses that decide WHICH documents make the page rather than how
/// they score. They reach pass 2 because that is where the selection
/// happens: a shard that picked its best `limit` by score and left the
/// re-ordering or the collapsing to the origin would hand back a page
/// missing the rows that should have been on it.
fn push_order(argv2: &mut Vec<Vec<u8>>, sort: Option<(Vec<u8>, bool)>, distinct: Option<Vec<u8>>) {
    if let Some((field, desc)) = sort {
        argv2.push(b"SORT".to_vec());
        argv2.push(field);
        argv2.push(if desc { b"DESC".to_vec() } else { b"ASC".to_vec() });
    }
    if let Some(field) = distinct {
        argv2.push(b"DISTINCT".to_vec());
        argv2.push(field);
    }
}

/// `FACET <field…>` — counted per shard over its whole match set and
/// summed here, so it travels to pass 2 like every other clause.
fn push_facets(argv2: &mut Vec<Vec<u8>>, facets: Vec<Vec<u8>>) {
    if !facets.is_empty() {
        argv2.push(b"FACET".to_vec());
        argv2.extend(facets);
    }
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
fn read_ranked_segment(c: &[u8], pos: &mut usize) -> Vec<(f64, Vec<u8>, Hydrated)> {
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
