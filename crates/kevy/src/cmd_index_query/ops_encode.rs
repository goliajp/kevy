//! Encoding one shard's ranked chunk for the origin. Split from `ops.rs`
//! for the 500-LOC house rule (a `#[path]` child, so it shares the op
//! module's wire helpers).

use kevy_store::Store;

use super::super::wire::{encode_highlight, encode_hydration};
use crate::cmd_index_query::ST_OK;

/// One hit's sort key on the wire: a present flag, then the bytes.
///
/// Absent is its own tag rather than an empty key, because an empty
/// stored value is a value and must not sort with the documents that have
/// none.
fn encode_okey(chunk: &mut Vec<u8>, okey: Option<&[u8]>) {
    match okey {
        Some(k) => {
            chunk.push(1);
            chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
            chunk.extend_from_slice(k);
        }
        None => chunk.push(0),
    }
}

/// same argv, so the two never drift.
pub(super) fn encode_hits(
    store: &mut Store,
    hits: &[kevy_text::TextMatch],
    spans: &Option<Vec<crate::cmd_index_query::HitSpans>>,
    okeys: &Option<Vec<Option<Vec<u8>>>>,
    dkeys: &Option<Vec<Option<Vec<u8>>>>,
    facets: &[Vec<kevy_text::Bucket>],
    fields: &[Vec<u8>],
) -> Vec<u8> {
    let mut chunk = vec![ST_OK];
    chunk.extend_from_slice(&(hits.len() as u32).to_le_bytes());
    for (i, h) in hits.iter().enumerate() {
        chunk.extend_from_slice(&(h.key.len() as u32).to_le_bytes());
        chunk.extend_from_slice(&h.key);
        chunk.extend_from_slice(&h.score.to_le_bytes());
        encode_hydration(store, &mut chunk, &h.key, fields);
        if let Some(spans) = &spans {
            encode_highlight(&mut chunk, &spans[i]);
        }
        if let Some(okeys) = okeys {
            encode_okey(&mut chunk, okeys[i].as_deref());
        }
        if let Some(dkeys) = dkeys {
            encode_okey(&mut chunk, dkeys[i].as_deref());
        }
    }
    // The facet buckets follow the hits: they are per query, not per hit,
    // and the reduce knows how many fields to expect from the same argv.
    for field in facets {
        chunk.extend_from_slice(&(field.len() as u32).to_le_bytes());
        for (key, label, n) in field {
            for part in [key, label] {
                chunk.extend_from_slice(&(part.len() as u32).to_le_bytes());
                chunk.extend_from_slice(part);
            }
            chunk.extend_from_slice(&n.to_le_bytes());
        }
    }
    chunk
}
