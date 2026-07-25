//! The clause-carrying scalar IDX.QUERY origin reduce: decode the
//! extended per-shard chunks (hits + per-hit sort/distinct keys + facet
//! buckets), run the SAME merge every embedding runs
//! ([`kevy_index::merge_claused`]), and emit the reply.
//!
//! Reply shape: the plain scalar envelope `[cursor, rows]` with the
//! cursor pinned to `"0"` (selection clauses refuse cursors), rows in
//! the merged order, and — exactly like the text surface — a `FACET`
//! query APPENDS one trailing element to the rows array.

use kevy_index::{FacetBucket, ScalarHit, fold_facets, merge_claused, sort_facets};
use kevy_resp::{encode_array_len, encode_bulk, encode_error};

use super::chunk::{emit_row, read_kbytes, read_u32, read_hydration, value_repr};
use crate::cmd_index_query::{Hydrated, Query, decode_value};

/// One merged hit with the hydration block that rode its chunk.
type HydratedHit = (ScalarHit, Hydrated);

/// The claused reduce — the caller has already established that the
/// argv parses and carries a selection clause.
pub(super) fn reduce_query_claused(q: &Query, chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut all: Vec<HydratedHit> = Vec::new();
    let mut facets: Vec<Vec<FacetBucket>> = vec![Vec::new(); q.facets.len()];
    for c in chunks {
        let pos = collect_hits(c, q, &mut all);
        collect_facets(c, pos, q.facets.len(), &mut facets);
    }
    let sort_desc = q.sort.as_ref().map(|(_, desc)| *desc);
    let all = merge_claused(all, sort_desc, q.distinct.is_some(), q.offset, q.limit);
    sort_facets(&mut facets);
    emit_reply(q, &all, &facets)
}

/// Decode one shard's hit block; returns the offset where the facet
/// block starts. A short/corrupt chunk stops that shard's contribution.
fn collect_hits(c: &[u8], q: &Query, all: &mut Vec<HydratedHit>) -> usize {
    let mut pos = 1usize;
    let Some(n) = read_u32(c, &mut pos) else { return pos };
    for _ in 0..n {
        let Some(key) = read_kbytes(c, &mut pos) else { break };
        let Some(value) = decode_value(c, &mut pos) else { break };
        let Some(fv) = read_hydration(c, &mut pos) else { break };
        let okey = if q.sort.is_some() {
            match read_okey(c, &mut pos) {
                Some(k) => k,
                None => break,
            }
        } else {
            None
        };
        let dkey = if q.distinct.is_some() {
            match read_okey(c, &mut pos) {
                Some(k) => k,
                None => break,
            }
        } else {
            None
        };
        all.push((ScalarHit { key, value, okey, dkey }, fv));
    }
    pos
}

/// The facet block after the hits: per requested field, its buckets as
/// `(identity, label, count)` — folded into the origin's running totals
/// by identity ([`fold_facets`]' contract).
fn collect_facets(c: &[u8], mut pos: usize, n_fields: usize, out: &mut [Vec<FacetBucket>]) {
    let mut part: Vec<Vec<FacetBucket>> = Vec::with_capacity(n_fields);
    for _ in 0..n_fields {
        let Some(n) = read_u32(c, &mut pos) else { return };
        let mut field = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let Some(id) = read_kbytes(c, &mut pos) else { return };
            let Some(label) = read_kbytes(c, &mut pos) else { return };
            let Some(cb) = c.get(pos..pos + 8) else { return };
            field.push((id, label, u64::from_le_bytes(cb.try_into().expect("8 bytes"))));
            pos += 8;
        }
        part.push(field);
    }
    fold_facets(out, part);
}

/// `[0]` = no usable value, `[1][len][bytes]` = the key.
fn read_okey(c: &[u8], pos: &mut usize) -> Option<Option<Vec<u8>>> {
    let tag = *c.get(*pos)?;
    *pos += 1;
    match tag {
        0 => Some(None),
        1 => Some(Some(read_kbytes(c, pos)?)),
        _ => None,
    }
}

/// The `[cursor, rows(+facets)]` envelope. Rows keep the plain query's
/// exact per-row shapes (flat `key value` pairs without FIELDS, hydrated
/// row arrays with); a FACET query's rows array gains ONE trailing
/// element, exactly like the text surface.
fn emit_reply(q: &Query, all: &[HydratedHit], facets: &[Vec<FacetBucket>]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_array_len(&mut out, 2);
    encode_bulk(&mut out, b"0");
    let extra = usize::from(!q.facets.is_empty());
    if q.fields.is_empty() {
        encode_array_len(&mut out, (all.len() * 2 + extra) as i64);
        for (h, _) in all {
            encode_bulk(&mut out, &h.key);
            encode_bulk(&mut out, &value_repr(&h.value));
        }
    } else {
        encode_array_len(&mut out, (all.len() + extra) as i64);
        for (h, fv) in all {
            emit_row(&mut out, &h.key, Some(&h.value), fv, &q.fields);
        }
    }
    emit_facet_element(&mut out, &q.facets, facets);
    out
}

/// The trailing facet element: `[field, [label, count, …], field, …]` —
/// buckets most frequent first, label breaking ties, byte-identical in
/// shape to the MATCH surface's.
fn emit_facet_element(out: &mut Vec<u8>, names: &[Vec<u8>], facets: &[Vec<FacetBucket>]) {
    if names.is_empty() {
        return;
    }
    encode_array_len(out, (names.len() * 2) as i64);
    for (name, field) in names.iter().zip(facets) {
        encode_bulk(out, name);
        encode_array_len(out, (field.len() * 2) as i64);
        for (_, label, n) in field {
            encode_bulk(out, label);
            encode_bulk(out, n.to_string().as_bytes());
        }
    }
}

/// The bad-parse fallback the dispatcher shares with the plain reduce.
pub(super) fn bad_args() -> Vec<u8> {
    let mut out = Vec::new();
    encode_error(&mut out, "ERR bad IDX arguments");
    out
}
