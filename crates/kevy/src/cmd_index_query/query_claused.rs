//! The clause-carrying scalar IDX.QUERY per-shard half — `FILTER` /
//! `SORT` / `DISTINCT` / `FACET` / `OFFSET` over the shard's segment.
//!
//! Chunk shapes:
//! * FILTER-only (cursor-compatible): the PLAIN scalar chunk —
//!   `[ST_OK][n][(klen,key,value,hydration)*]` — so the origin's k-way
//!   merge and global cursor work unchanged.
//! * Any selection clause: the same hit block plus, per hit, the sort /
//!   distinct keys the origin merge needs (present iff the query
//!   carried the clause — the reduce recovers that from the same argv),
//!   then per requested facet field its buckets.

use kevy_index::{ScalarClauses, ValueTest};
use kevy_store::Store;

use super::args::Query;
use super::ops_clauses::{distinct_field, facet_fields, filter_tests, sort_field};
use super::wire::{encode_hydration_row, encode_value, peek_hydration};
use super::{ST_BUILDING, ST_CLAUSE, ST_NOINDEX, ST_OK, ST_OVERBUDGET};
use crate::index_runtime;
use crate::state::Ctx;

/// The named refusal for a cursor riding a selection clause — checked
/// before the segment is even consulted, because it is pure grammar.
pub(super) const CURSOR_CLAUSE_CONFLICT: &str =
    "CURSOR cannot combine with SORT|DISTINCT|FACET|OFFSET";

/// A ready-made ST_CLAUSE chunk for `msg`.
pub(super) fn clause_chunk(msg: &str) -> Vec<u8> {
    let mut chunk = vec![ST_CLAUSE];
    chunk.extend_from_slice(msg.as_bytes());
    chunk
}

/// Run a clause-carrying scalar query against this shard's segment and
/// encode the chunk. The caller has already refused CURSOR × selection.
pub(super) fn run_claused_query(ctx: &Ctx<'_>, store: &mut Store, q: &Query) -> Vec<u8> {
    let res = index_runtime::with_ready_segment(ctx, store, &q.name, |spec, seg| {
        let (min, max) = q.bounds_for(spec)?;
        let filters: Vec<(usize, ValueTest)> = filter_tests(spec, &q.filters)?;
        let sort = sort_field(spec, &q.sort)?;
        let distinct = distinct_field(spec, &q.distinct)?;
        let facets = facet_fields(spec, &q.facets)?;
        let clauses = ScalarClauses {
            filters: &filters,
            sort,
            distinct,
            facets: &facets,
            // Each shard returns limit+offset: the origin drains the
            // offset AFTER the merge, and a shard cannot know which of
            // its hits survive it.
            fetch: q.limit + q.offset,
        };
        let cursor = q.cursor(spec.ty);
        Ok(seg.query_claused(&min, &max, cursor.as_ref(), &clauses))
    });
    match res {
        Ok(Err(chunk)) => chunk,
        Ok(Ok(page)) => encode_claused_chunk(store, q, &page),
        Err(e) if e.as_wire().starts_with("INDEXBUILDING") => vec![ST_BUILDING],
        Err(e) if e.as_wire().starts_with("INDEXOVERBUDGET") => vec![ST_OVERBUDGET],
        Err(_) => vec![ST_NOINDEX],
    }
}

/// Hit block (+ per-hit clause keys when the query carried the clause),
/// then the facet block. Hydration happens outside the segment borrow —
/// the hits' rows live on this shard, plain hash reads.
fn encode_claused_chunk(
    store: &mut Store,
    q: &Query,
    page: &kevy_index::ClausedPage,
) -> Vec<u8> {
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(page.hits.len() as u32).to_le_bytes());
    // Hydration rows prefetched as ONE batched page (cold rows
    // coalesce into one submission), then encoded in hit order.
    let keys: Vec<&[u8]> = page.hits.iter().map(|h| h.key.as_slice()).collect();
    let rows = peek_hydration(store, &keys, &q.fields);
    for (i, h) in page.hits.iter().enumerate() {
        chunk.extend_from_slice(&(h.key.len() as u32).to_le_bytes());
        chunk.extend_from_slice(&h.key);
        encode_value(&mut chunk, &h.value);
        encode_hydration_row(&mut chunk, q.fields.len(), &rows[i]);
        if q.sort.is_some() {
            encode_okey(&mut chunk, h.okey.as_deref());
        }
        if q.distinct.is_some() {
            encode_okey(&mut chunk, h.dkey.as_deref());
        }
    }
    for field in &page.facets {
        chunk.extend_from_slice(&(field.len() as u32).to_le_bytes());
        for (id, label, n) in field {
            chunk.extend_from_slice(&(id.len() as u32).to_le_bytes());
            chunk.extend_from_slice(id);
            chunk.extend_from_slice(&(label.len() as u32).to_le_bytes());
            chunk.extend_from_slice(label);
            chunk.extend_from_slice(&n.to_le_bytes());
        }
    }
    chunk
}

/// `[0]` = no usable value, `[1][len][bytes]` = the key (the ranked
/// chunk's okey convention).
fn encode_okey(chunk: &mut Vec<u8>, k: Option<&[u8]>) {
    match k {
        None => chunk.push(0),
        Some(b) => {
            chunk.push(1);
            chunk.extend_from_slice(&(b.len() as u32).to_le_bytes());
            chunk.extend_from_slice(b);
        }
    }
}
