//! `IDX.VERIFY` on a global index, a shard's half. The rows and their
//! entries live on different shards, so neither side can recheck the other
//! alone: each shard reports the entries it holds as an owner and the
//! entries its rows call for, both as `(key, partition, entry hash)`, and
//! the origin matches the two lists.
//!
//! Chunk: `[ST_OK]['G'][entries][bytes][coerce_failures][duplicates]`
//! (u64 each), then the held list and the owed list, each `n u32` then
//! `n × (len u32, key, partition u16, hash u64)`.

use kevy_index::IndexSpec;
use kevy_store::Store;

use super::global::{GlobalRole, derive, entry_hash};
use crate::state::Ctx;

/// The tag after the status byte of a global index's VERIFY chunk.
pub(crate) const VERIFY_TAG: u8 = b'G';

/// One entry as VERIFY compares it.
pub(crate) type Placed = (Vec<u8>, u16, u64);

/// This shard's VERIFY chunk for global index `name`; `None` when `name`
/// is not a global index here.
pub(crate) fn verify_chunk(ctx: &Ctx<'_>, store: &mut Store, name: &[u8]) -> Option<Vec<u8>> {
    let mut st = ctx.shard.indexes.borrow_mut();
    super::refresh(ctx, &mut st);
    let si = st.idx.iter().find(|si| si.spec.name() == name)?;
    let g = si.global.as_ref()?;
    if !g.ready() {
        return Some(vec![crate::cmd_index_query::ST_BUILDING]);
    }
    let (held, stats) = held(g);
    let (owed, coerce_failures) = owed(store, &si.spec, g);
    let mut chunk = vec![crate::cmd_index_query::ST_OK, VERIFY_TAG];
    for n in [stats.0, stats.1, coerce_failures, stats.2] {
        chunk.extend_from_slice(&n.to_le_bytes());
    }
    put_list(&mut chunk, &held);
    put_list(&mut chunk, &owed);
    Some(chunk)
}

/// The entries this shard's partitions hold, and their `(entries, bytes,
/// duplicates)` — exact for a unique index, since every entry of a value
/// lives in the one partition that value falls in.
fn held(g: &GlobalRole) -> (Vec<Placed>, (u64, u64, u64)) {
    let (mut out, mut stats) = (Vec::new(), (0, 0, 0));
    for (p, seg) in &g.owned {
        let mut scan = seg.scan(None, kevy_index::SortOrder::Asc);
        while let Some((v, k)) = scan.next_entry() {
            let (enc, k) = (v.order_bytes(), k.to_vec());
            let vals = scan.stored_row();
            let h = entry_hash(&enc, vals.iter().map(|x| x.as_deref()));
            out.push((k, *p as u16, h));
        }
        let s = seg.stats();
        stats = (stats.0 + s.entries, stats.1 + s.approx_bytes, stats.2 + s.duplicates);
    }
    (out, stats)
}

/// The entries this shard's rows call for, derived from the rows as they
/// are now, and how many rows under the prefix derive none.
fn owed(store: &mut Store, spec: &IndexSpec, g: &GlobalRole) -> (Vec<Placed>, u64) {
    let mut pat = spec.prefix().to_vec();
    pat.push(b'*');
    let (mut out, mut excluded) = (Vec::new(), 0);
    for key in store.collect_keys(Some(&pat), None) {
        match derive(store, spec, &key) {
            Some((v, vals)) => {
                let enc = v.order_bytes();
                let p = g.part.partition_of(&enc) as u16;
                out.push((key, p, entry_hash(&enc, vals.iter().map(|v| v.as_deref()))));
            }
            None => excluded += 1,
        }
    }
    (out, excluded)
}

fn put_list(chunk: &mut Vec<u8>, list: &[Placed]) {
    chunk.extend_from_slice(&(list.len() as u32).to_le_bytes());
    for (k, p, h) in list {
        chunk.extend_from_slice(&(k.len() as u32).to_le_bytes());
        chunk.extend_from_slice(k);
        chunk.extend_from_slice(&p.to_le_bytes());
        chunk.extend_from_slice(&h.to_le_bytes());
    }
}
