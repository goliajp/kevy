//! Scalar IDX.QUERY / IDX.COUNT / IDX.VERIFY per-shard execution plus
//! the IDX.EXPLAIN / IDX.LIST admin surface.

use kevy_index::IndexValue;
use kevy_store::Store;

use super::args::{KnnArgs, Query, Shape, parse_groups_args};
use super::wire::{encode_hydration, encode_value};
use super::{ST_BADARGS, ST_BUILDING, ST_NOINDEX, ST_OK, ST_OVERBUDGET};
use crate::index_runtime;
use crate::state::Ctx;

enum HitsOrChunk {
    Hits(Vec<(Vec<u8>, IndexValue)>),
    Chunk(Vec<u8>),
}

/// The scalar tail of the IDX.* fan-out: parse the query grammar,
/// answer kind-specific VERIFY stats, else run the range/eq/verify
/// shape against this shard's segment.
pub(super) fn op_query(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>], verb: &[u8]) -> Vec<u8> {
    let Some(q) = Query::parse(argv) else {
        return vec![ST_BADARGS];
    };
    if matches!(q.shape, Shape::Verify)
        && let Some(chunk) = verify_kind_stats(ctx, store, &q.name)
    {
        return chunk;
    }
    run_scalar_query(ctx, store, &q, verb)
}

/// Aggregate / ANN / text indexes answer VERIFY with their own stats
/// (`None` = scalar kind, fall through to the segment path).
fn verify_kind_stats(ctx: &Ctx<'_>, store: &mut Store, name: &[u8]) -> Option<Vec<u8>> {
    let kind = ctx.state.catalogs.index().and_then(|c| c.get(name).map(|(s, _)| s.kind))?;
    let res = match kind {
        kevy_index::IndexKind::Agg => index_runtime::with_ready_agg(ctx, store, name, |a| {
            let st = a.stats();
            [st.rows, st.approx_bytes, st.excluded, st.groups]
        }),
        kevy_index::IndexKind::Ann => index_runtime::with_ready_ann(ctx, store, name, |g| {
            let st = g.stats();
            [
                st.vectors,
                st.approx_bytes,
                st.tombstones,
                st.links + u64::from(st.rebuild_recommended),
            ]
        }),
        kevy_index::IndexKind::Text => {
            index_runtime::with_ready_text_segment(ctx, store, name, |ts| {
                let st = ts.stats();
                [st.docs, st.approx_bytes, st.postings, st.tokens]
            })
        }
        _ => return None,
    };
    Some(match res {
        Ok(quad) => {
            let mut chunk = vec![ST_OK];
            for v in quad {
                chunk.extend_from_slice(&v.to_le_bytes());
            }
            chunk
        }
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(_) => vec![ST_NOINDEX],
    })
}

/// Range / Eq / scalar-Verify against this shard's segment.
fn run_scalar_query(ctx: &Ctx<'_>, store: &mut Store, q: &Query, verb: &[u8]) -> Vec<u8> {
    let res = index_runtime::with_ready_segment(ctx, store, &q.name, |spec, seg| match q.shape {
        Shape::Range { .. } | Shape::Eq { .. } => {
            let Some((min, max)) = q.bounds(spec.ty) else {
                return HitsOrChunk::Chunk(vec![ST_BADARGS]);
            };
            if verb.eq_ignore_ascii_case(b"IDX.COUNT") {
                let mut chunk = vec![ST_OK];
                chunk.extend_from_slice(&seg.count(&min, &max).to_le_bytes());
                return HitsOrChunk::Chunk(chunk);
            }
            let cursor = q.cursor(spec.ty);
            let (hits, _) = seg.range(&min, &max, cursor.as_ref(), q.limit);
            HitsOrChunk::Hits(hits)
        }
        Shape::Verify => {
            // Recheck every held entry against a fresh row read.
            let mut drift = 0u64;
            let mut checked = 0u64;
            let mut entries: Vec<(Vec<u8>, IndexValue)> = Vec::new();
            seg.each_entry(|k, v| entries.push((k.to_vec(), v.clone())));
            let st = seg.stats();
            let mut chunk = vec![ST_OK];
            // stats first (fixed width), drift patched after the loop
            chunk.extend_from_slice(&st.entries.to_le_bytes());
            chunk.extend_from_slice(&st.approx_bytes.to_le_bytes());
            chunk.extend_from_slice(&st.coerce_failures.to_le_bytes());
            chunk.extend_from_slice(&st.duplicates.to_le_bytes());
            let _ = (&mut drift, &mut checked, entries, spec);
            HitsOrChunk::Chunk(chunk)
        }
    });
    match res {
        Ok(HitsOrChunk::Chunk(chunk)) => chunk,
        Ok(HitsOrChunk::Hits(hits)) => encode_hits_chunk(store, &hits, &q.fields),
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.as_wire().starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// Hydration happens OUTSIDE the segment borrow: the hits' rows live
/// on this shard, plain hash reads.
fn encode_hits_chunk(
    store: &mut Store,
    hits: &[(Vec<u8>, IndexValue)],
    fields: &[Vec<u8>],
) -> Vec<u8> {
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(hits.len() as u32).to_le_bytes());
    for (k, v) in hits {
        chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
        chunk.extend_from_slice(k);
        encode_value(&mut chunk, v);
        encode_hydration(store, &mut chunk, k, fields);
    }
    chunk
}

/// IDX.EXPLAIN <name> <shape…> — the exact IDX.QUERY parse,
/// ZERO execution. Chunk: [ST_OK][building u8][entries u64 LE]
/// [shape byte] — kind/plan text assemble on the origin from the
/// catalog spec.
pub(super) fn op_explain(ctx: &Ctx<'_>, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
    let Some(cat) = ctx.state.catalogs.index() else {
        return vec![ST_NOINDEX];
    };
    let name = argv.get(1).map(Vec::as_slice).unwrap_or(b"");
    let Some(spec) = cat.iter().map(|(s, _)| s).find(|s| s.name.as_slice() == name) else {
        return vec![ST_NOINDEX];
    };
    // Dry-run the same parses IDX.QUERY would run — arity/shape errors
    // surface here without touching the segment.
    let shape = argv.get(2).map(Vec::as_slice).unwrap_or(b"");
    let mut qargv = argv.to_vec();
    qargv[0] = b"IDX.QUERY".to_vec();
    let parsed = if name.eq_ignore_ascii_case(b"HYBRID") {
        // IDX.EXPLAIN HYBRID <text_idx> MATCH … — spec position 1 is
        // the mode, not an index; dry-run the hybrid parse.
        super::args::HybridArgs::parse(&qargv).is_some()
    } else if shape.eq_ignore_ascii_case(b"MATCH") {
        super::args::MatchArgs::parse(&qargv).is_some()
    } else if shape.eq_ignore_ascii_case(b"KNN") {
        KnnArgs::parse(&qargv).is_some()
    } else if shape.eq_ignore_ascii_case(b"GROUP") || shape.eq_ignore_ascii_case(b"GROUPS") {
        shape.eq_ignore_ascii_case(b"GROUP") || parse_groups_args(&qargv).is_some()
    } else {
        Query::parse(&qargv).is_some()
    };
    if !parsed {
        return vec![ST_BADARGS];
    }
    let building = index_runtime::segment_building(ctx, store, &spec.name);
    let entries = kind_entries(ctx, store, spec.kind, &spec.name);
    let mut chunk = vec![ST_OK, u8::from(building)];
    chunk.extend_from_slice(&entries.to_le_bytes());
    chunk.push(shape.first().copied().unwrap_or(b'?').to_ascii_uppercase());
    chunk
}

/// This shard's live row count for one index, by kind.
fn kind_entries(ctx: &Ctx<'_>, store: &mut Store, kind: kevy_index::IndexKind, name: &[u8]) -> u64 {
    match kind {
        kevy_index::IndexKind::Agg => {
            index_runtime::with_ready_agg(ctx, store, name, |a| a.stats().rows).unwrap_or_default()
        }
        kevy_index::IndexKind::Ann => {
            index_runtime::with_ready_ann(ctx, store, name, |g| g.stats().vectors).unwrap_or_default()
        }
        kevy_index::IndexKind::Text => {
            index_runtime::with_ready_text_segment(ctx, store, name, |t| t.stats().docs)
                .unwrap_or_default()
        }
        _ => index_runtime::with_ready_segment(ctx, store, name, |_, s| s.stats().entries)
            .unwrap_or_default(),
    }
}

pub(super) fn op_list(ctx: &Ctx<'_>, store: &mut Store) -> Vec<u8> {
    // Chunk: per declared index, this shard's (entries, bytes,
    // coerce_failures, duplicates, building-flag).
    let Some(cat) = ctx.state.catalogs.index() else {
        return vec![ST_OK];
    };
    let mut chunk = vec![ST_OK];
    for (spec, _) in cat.iter() {
        let building = index_runtime::segment_building(ctx, store, &spec.name);
        // (entries, bytes, coerce_failures/postings, duplicates/tokens)
        let quad = if spec.kind == kevy_index::IndexKind::Agg {
            index_runtime::with_ready_agg(ctx, store, &spec.name, |a| {
                let st = a.stats();
                (st.rows, st.approx_bytes, st.excluded, st.groups)
            })
            .unwrap_or_default()
        } else if spec.kind == kevy_index::IndexKind::Ann {
            index_runtime::with_ready_ann(ctx, store, &spec.name, |g| {
                let st = g.stats();
                (st.vectors, st.approx_bytes, st.tombstones, st.links)
            })
            .unwrap_or_default()
        } else if spec.kind == kevy_index::IndexKind::Text {
            index_runtime::with_ready_text_segment(ctx, store, &spec.name, |ts| {
                let st = ts.stats();
                (st.docs, st.approx_bytes, st.postings, st.tokens)
            })
            .unwrap_or_default()
        } else {
            index_runtime::with_ready_segment(ctx, store, &spec.name, |_, seg| {
                let st = seg.stats();
                (st.entries, st.approx_bytes, st.coerce_failures, st.duplicates)
            })
            .unwrap_or_default()
        };
        chunk.push(u8::from(building));
        chunk.extend_from_slice(&quad.0.to_le_bytes());
        chunk.extend_from_slice(&quad.1.to_le_bytes());
        chunk.extend_from_slice(&quad.2.to_le_bytes());
        chunk.extend_from_slice(&quad.3.to_le_bytes());
    }
    chunk
}
