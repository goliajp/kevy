//! Ranked / aggregate / compose per-shard halves (MATCH, KNN, HYBRID,
//! GROUP/GROUPS, AGG.FETCH, COMPOSE).

use kevy_index::{IndexValue, ValType};
use kevy_store::Store;

use super::args::{ComposeQuery, HybridArgs, KnnArgs, MatchArgs, Shape};
use super::wire::{decode_gstats_arg, encode_agg_chunk, encode_hydration, encode_stats_chunk};
use super::{ST_BADARGS, ST_BUILDING, ST_NOINDEX, ST_OK, ST_OVERBUDGET};
use crate::index_runtime;
use crate::state::Ctx;

/// Text MATCH pass 1 (per-shard): report this shard's corpus counters
/// so the reduce can build one global [`kevy_text::CorpusStats`] and a
/// hit's BM25 rank stops depending on which shard it landed on (global
/// BM25, step 4b-server; the embedded twin is `idx_match`). Chunk:
/// `[ST_OK][n_docs u64][total_len u64][ntok u32][(tlen,token,df u32)*]`.
///
/// The terminal-surface validation runs HERE (pass 1) so a NOTYET/BADARGS
/// answer is returned before any second fan-out — the syntax verdict must
/// not wait on a scoring round.
pub(super) fn op_match(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let q = match MatchArgs::parse_terminal(argv) {
        crate::cmd_index_query::args::MatchParse::Ok(q) => q,
        crate::cmd_index_query::args::MatchParse::BadArgs => return vec![ST_BADARGS],
        crate::cmd_index_query::args::MatchParse::NotYet(clause) => {
            let mut chunk = vec![crate::cmd_index_query::ST_NOTYET];
            chunk.extend_from_slice(clause);
            return chunk;
        }
    };
    let mut q_tokens = kevy_text::tokenize(&q.text);
    q_tokens.sort();
    q_tokens.dedup();
    let res = index_runtime::with_ready_text_segment(ctx, store, &q.name, |ts| {
        let tokdf: Vec<(Vec<u8>, u32)> =
            q_tokens.iter().map(|t| (t.clone(), ts.local_df(t))).collect();
        (ts.stats().docs, ts.total_len(), tokdf)
    });
    match res {
        Ok((n_docs, total_len, tokdf)) => {
            let mut chunk = Vec::new();
            encode_stats_chunk(&mut chunk, n_docs, total_len, &tokdf);
            chunk
        }
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.as_wire().starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// Text MATCH pass 2 (internal `MATCH.SCORE`): score this shard against
/// the injected global stats and emit ranked hits + owning-shard
/// hydration. Chunk `[ST_OK][n][(klen,key,score f64,fcount,fields)*]` —
/// same layout as KNN, so the reduce shares the decoder.
///
/// argv: `[MATCH.SCORE, name, text, LIMIT=<n>, <gstats>, (FIELDS f…)?]`.
pub(super) fn op_match_score(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some((name, text, limit, fields)) = super::args::parse_match_score(argv) else {
        return vec![ST_BADARGS];
    };
    let Some(stats) = argv.get(4).and_then(|b| decode_gstats_arg(b)) else {
        return vec![ST_BADARGS];
    };
    let res = index_runtime::with_ready_text_segment(ctx, store, &name, |ts| {
        ts.matches_scored(&text, limit, Some(&stats))
    });
    match res {
        Ok(hits) => {
            let mut chunk = vec![ST_OK];
            chunk.extend_from_slice(&(hits.len() as u32).to_le_bytes());
            for h in &hits {
                chunk.extend_from_slice(&(h.key.len() as u32).to_le_bytes());
                chunk.extend_from_slice(&h.key);
                chunk.extend_from_slice(&h.score.to_le_bytes());
                encode_hydration(store, &mut chunk, &h.key, &fields);
            }
            chunk
        }
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.as_wire().starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// Agg per-shard. Chunk (both shapes):
/// `[ST_OK][n][(glen,group,count u64,sum f64,minflag+min,maxflag+max)*]`
/// — GROUP sends the one requested group; GROUPS sends every local
/// group (the reduce needs full partials to merge exactly).
pub(super) fn op_agg(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let single = argv[2].eq_ignore_ascii_case(b"GROUP");
    if single {
        let res = index_runtime::with_ready_agg(ctx, store, &argv[1], |a| {
            argv.get(3).map(|g| vec![(g.clone(), a.group(g))])
        });
        return match res {
            Ok(None) => vec![ST_BADARGS],
            Ok(Some(rows)) => encode_agg_chunk(&rows),
            Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
            Err(e) if e.as_wire().starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
            Err(_) => vec![ST_NOINDEX],
        };
    }
    // GROUPS phase 1 (distributed exact top-K, TPUT-style): send only
    // the local top-(4×limit) by the ranking metric plus an
    // `exhausted` flag (a shard that sent EVERYTHING has τ = nothing
    // unsent — the reduce's uncertainty test needs to know). Full
    // materialization measured 14-18ms at 8×10k groups; this keeps
    // chunks at ~4·limit rows.
    let Some((by, limit)) = super::args::parse_groups_args(argv) else {
        return vec![ST_BADARGS];
    };
    let depth: usize = argv
        .iter()
        .find_map(|a| std::str::from_utf8(a).ok()?.strip_prefix("DEPTH=")?.parse().ok())
        .unwrap_or(1);
    let res = index_runtime::with_ready_agg(ctx, store, &argv[1], |a| {
        if depth == 0 {
            // fallback sentinel: full local materialization (uniform
            // near-tie data is unprunable — see reduce_agg)
            return (a.all_groups(), true);
        }
        let fetch = (limit * 4 * depth).max(64 * depth);
        let rows = a.top_groups(by, fetch);
        let exhausted = rows.len() < fetch;
        (rows, exhausted)
    });
    match res {
        Ok((rows, exhausted)) => {
            let mut chunk = encode_agg_chunk(&rows);
            chunk.push(u8::from(exhausted));
            chunk
        }
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.as_wire().starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// Phase 2 of GROUPS (internal): `AGG.FETCH <name> <g…>` — exact partials
/// for the candidate groups that survived phase-1 ranking.
pub(super) fn op_agg_fetch(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let res = index_runtime::with_ready_agg(ctx, store, &argv[1], |a| {
        argv[2..].iter().map(|g| (g.clone(), a.group(g))).collect::<Vec<_>>()
    });
    match res {
        Ok(rows) => encode_agg_chunk(&rows),
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// `IDX.REBUILD <name>` (ANN tombstone compaction).
pub(super) fn op_rebuild(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(name) = argv.get(1) else {
        return vec![ST_BADARGS];
    };
    match index_runtime::with_ready_ann(ctx, store, name, |g| g.rebuild()) {
        Ok(()) => vec![ST_OK],
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// KNN per-shard: distance-ranked hits + hydration. Chunk:
/// `[ST_OK][n][(klen,key,dist f64,fcount,fields)*]` — same layout as
/// MATCH chunks, so the reduce shares the decoder.
pub(super) fn op_knn(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(q) = KnnArgs::parse(argv) else {
        return vec![ST_BADARGS];
    };
    let res = index_runtime::with_ready_ann(ctx, store, &q.name, |g| {
        kevy_vector::parse_vector(&q.vec, g.dim()).map(|v| g.knn(&v, q.limit, q.ef))
    });
    match res {
        Ok(None) => vec![ST_BADARGS], // vector doesn't match DIM
        Ok(Some(hits)) => {
            let mut chunk = vec![ST_OK];
            chunk.extend_from_slice(&(hits.len() as u32).to_le_bytes());
            for (key, dist) in &hits {
                chunk.extend_from_slice(&(key.len() as u32).to_le_bytes());
                chunk.extend_from_slice(key);
                chunk.extend_from_slice(&f64::from(*dist).to_le_bytes());
                encode_hydration(store, &mut chunk, key, &q.fields);
            }
            chunk
        }
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.as_wire().starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// Per-shard HYBRID: run BOTH sub-queries at 4×limit depth (rank
/// fusion needs deeper lists than the final cut — same 4× posture as
/// the agg TPUT phase-1), emit two ranked segments back to back.
/// Chunk: `[ST_OK][match n][(key,f64,hydration)*][knn n][(…)*]`.
pub(super) fn op_hybrid(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(q) = HybridArgs::parse(argv) else {
        return vec![ST_BADARGS];
    };
    let depth = q.limit * 4;
    let m = index_runtime::with_ready_text_segment(ctx, store, &q.text_idx, |ts| {
        ts.matches(&q.text, depth)
    });
    let k = index_runtime::with_ready_ann(ctx, store, &q.ann_idx, |g| {
        kevy_vector::parse_vector(&q.vec, g.dim()).map(|v| g.knn(&v, depth, q.ef))
    });
    let (m, k) = match (m, k) {
        (Ok(m), Ok(Some(k))) => (m, k),
        (_, Ok(None)) => return vec![ST_BADARGS],
        (Err(e), _) | (_, Err(e)) if e.as_wire().starts_with("INDEXBUILDING") => {
            return vec![ST_BUILDING];
        }
        (Err(e), _) | (_, Err(e)) if e.as_wire().starts_with("INDEXOVERBUDGET") => {
            return vec![ST_OVERBUDGET];
        }
        _ => return vec![ST_NOINDEX],
    };
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(m.len() as u32).to_le_bytes());
    for h in &m {
        chunk.extend_from_slice(&(h.key.len() as u32).to_le_bytes());
        chunk.extend_from_slice(&h.key);
        chunk.extend_from_slice(&h.score.to_le_bytes());
        encode_hydration(store, &mut chunk, &h.key, &q.fields);
    }
    chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
    for (key, dist) in &k {
        chunk.extend_from_slice(&(key.len() as u32).to_le_bytes());
        chunk.extend_from_slice(key);
        chunk.extend_from_slice(&f64::from(*dist).to_le_bytes());
        encode_hydration(store, &mut chunk, key, &q.fields);
    }
    chunk
}

/// Per-shard COMPOSE: both sub-queries run against THIS shard's
/// segments (a key lives on exactly one shard, so per-shard set
/// algebra composes globally). Key-ordered; cursor = key point.
pub(super) fn op_compose(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(cq) = ComposeQuery::parse(argv) else {
        return vec![ST_BADARGS];
    };
    let res = index_runtime::with_two_ready_segments(
        ctx,
        store,
        &cq.a.name,
        &cq.b.name,
        |spec_a, seg_a, spec_b, seg_b| compose_keys(&cq, spec_a.ty, seg_a, spec_b.ty, seg_b),
    );
    match res {
        Ok(Some(keys)) => {
            let mut chunk = vec![ST_OK];
            chunk.extend_from_slice(&(keys.len() as u32).to_le_bytes());
            for k in &keys {
                chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
                chunk.extend_from_slice(k);
                encode_hydration(store, &mut chunk, k, &cq.fields);
            }
            chunk
        }
        Ok(None) => vec![ST_BADARGS],
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.as_wire().starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// The COMPOSE set algebra over two READY segments: AND filters the
/// A-hits through B's held entries; OR unions both ranges. Key-sorted,
/// cursor-trimmed, truncated to the limit.
/// `LIMIT` does NOT bound the work here, and cannot.
///
/// Every other shape in this surface is cursor-paged and limit-bounded. This
/// one is not, and the reason is structural rather than an oversight: a
/// segment is a `BTreeSet<(value, key)>` — ordered by VALUE — while COMPOSE's
/// result and its cursor are ordered by KEY. Producing one key-ordered page
/// therefore requires the whole match set, so a `COMPOSE OR` over two broad
/// ranges pays for both ranges plus a sort on every page even at `LIMIT 10`.
///
/// This is a cost model, not a bug, and the command reference states it. The
/// only way to bound it would be to page in value order of the driving leaf,
/// which is a different (and less useful) contract.
fn compose_keys(
    cq: &ComposeQuery,
    ty_a: ValType,
    seg_a: &kevy_index::Segment,
    ty_b: ValType,
    seg_b: &kevy_index::Segment,
) -> Option<Vec<Vec<u8>>> {
    let (min_a, max_a) = sub_bounds(&cq.a.shape, ty_a)?;
    let (min_b, max_b) = sub_bounds(&cq.b.shape, ty_b)?;
    // usize::MAX is deliberate — see the note above: a key-ordered page needs
    // the full match set out of a value-ordered index.
    let (a_hits, _) = seg_a.range(&min_a, &max_a, None, usize::MAX);
    let mut keys: Vec<Vec<u8>> = if cq.and {
        a_hits
            .into_iter()
            .filter(|(k, _)| {
                seg_b
                    .verify_entry(k)
                    .is_some_and(|v| *v >= min_b && *v <= max_b)
            })
            .map(|(k, _)| k)
            .collect()
    } else {
        let (b_hits, _) = seg_b.range(&min_b, &max_b, None, usize::MAX);
        let mut all: Vec<Vec<u8>> =
            a_hits.into_iter().chain(b_hits).map(|(k, _)| k).collect();
        all.sort();
        all.dedup();
        all
    };
    keys.sort();
    if let Some(cur) = &cq.cursor_key {
        keys.retain(|k| k.as_slice() > cur.as_slice());
    }
    keys.truncate(cq.limit);
    Some(keys)
}

fn sub_bounds(shape: &Shape, ty: ValType) -> Option<(IndexValue, IndexValue)> {
    match shape {
        Shape::Range { min, max } => Some((
            IndexValue::parse_literal(ty, min)?,
            IndexValue::parse_literal(ty, max)?,
        )),
        Shape::Eq { value } => {
            let v = IndexValue::parse_literal(ty, value)?;
            Some((v.clone(), v))
        }
        Shape::Verify => None,
    }
}
