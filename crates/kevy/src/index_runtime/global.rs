//! Global indexes: one index spread over the shards by value order. A row
//! stays on the shard its key hashes to, but its index entry lives in the
//! partition its value falls in, on the shard that partition's owner is.
//!
//! A write knows the row's entry before and after (the store's record of
//! the row before the write), so the row's shard sends nothing when the
//! entry did not change, one upsert naming the old value when it stayed in
//! its partition, and a delete plus an upsert when it moved. The old value
//! is what the owner finds the stale entry by. The messages leave through
//! the runtime's hook-message channel; a client's write waits for them to
//! be applied before it replies.
//!
//! A partition is built from every shard's rows. Each shard, once its
//! backfill has sent its last entry, tells every owner so; messages from
//! one shard to another arrive in order, so an owner that has heard from
//! all N holds every entry, and answers reads only then.

use kevy_index::{IndexSpec, IndexValue, Partitioning, Segment, partition_owner};
use kevy_store::Store;

use crate::state::Ctx;

/// One entry change, as the row's shard sends it to a partition's owner.
#[derive(Debug, PartialEq)]
pub(crate) enum Delta {
    /// The row, held under `value`, left the partition.
    Delete { key: Vec<u8>, value: IndexValue },
    /// The row's entry in the partition is now this; it was held under
    /// `old` when it was in the partition already.
    Upsert {
        key: Vec<u8>,
        old: Option<IndexValue>,
        value: IndexValue,
        values: Vec<Option<Vec<u8>>>,
    },
    /// Shard `from` has sent the entries of every row it held when the
    /// index was created.
    Built { from: usize },
}

/// This shard's part in one global index.
#[derive(Debug)]
pub(crate) struct GlobalRole {
    pub(super) part: Partitioning,
    shard: usize,
    nshards: usize,
    /// The catalog's incarnation of this index; messages for another are
    /// dropped.
    pub(crate) inc: u64,
    /// Per shard, whether it has sent all its rows' entries.
    built: Vec<bool>,
    /// The partitions this shard owns, each with its entries.
    pub(crate) owned: Vec<(usize, Segment)>,
    /// Messages waiting for the runtime to take.
    pub(crate) outbox: Vec<(usize, Vec<u8>)>,
}

impl GlobalRole {
    pub(crate) fn new(spec: &IndexSpec, part: &Partitioning, at: (usize, usize), inc: u64) -> Self {
        let (shard, nshards) = at;
        let owned = (0..part.partitions())
            .filter(|&p| partition_owner(spec.name(), p, nshards) == shard)
            .map(|p| (p, super::new_scalar_seg(spec)))
            .collect();
        GlobalRole {
            part: part.clone(),
            shard,
            nshards,
            inc,
            built: vec![false; nshards],
            owned,
            outbox: Vec::new(),
        }
    }

    /// Whether this role is incarnation `inc`, on this shard layout.
    pub(crate) fn fits(&self, at: (usize, usize), inc: u64) -> bool {
        (self.shard, self.nshards) == at && self.inc == inc
    }

    /// Whether this shard's partition holds every row's entry: every shard
    /// has finished sending (a shard owning none has nothing to wait for).
    pub(crate) fn ready(&self) -> bool {
        self.owned.is_empty() || self.built.iter().all(|&b| b)
    }

    /// This shard's backfill has sent every entry: tell each owner.
    pub(crate) fn finish_build(&mut self, spec: &IndexSpec) {
        for p in 0..self.part.partitions() {
            self.send(spec, p as u16, Delta::Built { from: self.shard });
        }
    }

    /// The row at `key` was written (or removed), and held `old` before:
    /// queue what its entry's partition owners must apply.
    pub(crate) fn on_row(
        &mut self,
        store: &mut Store,
        spec: &IndexSpec,
        key: &[u8],
        old: super::row_apply::OldEntry,
    ) {
        let prev = old.map(|(v, vals)| {
            let enc = v.order_bytes();
            let p = self.part.partition_of(&enc) as u16;
            (p, entry_hash(&enc, vals.iter().map(|x| x.as_deref())), v)
        });
        let Some((value, values)) = derive(store, spec, key) else {
            if let Some((p, _, v)) = prev {
                self.send(spec, p, Delta::Delete { key: key.to_vec(), value: v });
            }
            return;
        };
        let enc = value.order_bytes();
        let p = self.part.partition_of(&enc) as u16;
        let h = entry_hash(&enc, values.iter().map(|v| v.as_deref()));
        let old = match prev {
            Some((q, g, _)) if (q, g) == (p, h) => return,
            Some((q, _, v)) if q != p => {
                self.send(spec, q, Delta::Delete { key: key.to_vec(), value: v });
                None
            }
            Some((_, _, v)) => Some(v),
            None => None,
        };
        self.send(spec, p, Delta::Upsert { key: key.to_vec(), old, value, values });
    }

    /// Every row and entry gone (FLUSHALL / FLUSHDB runs on every shard).
    pub(crate) fn clear(&mut self, spec: &IndexSpec) {
        for (_, seg) in &mut self.owned {
            *seg = super::new_scalar_seg(spec);
        }
    }

    /// Apply a delta a row's shard sent for partition `p`.
    pub(crate) fn apply(&mut self, p: usize, delta: Delta) {
        if let Delta::Built { from } = delta {
            let was = self.ready();
            if let Some(b) = self.built.get_mut(from) {
                *b = true;
            }
            if !was && self.ready() {
                // every shard has sent its rows: pack what arrived in hash order
                for (_, seg) in &mut self.owned {
                    seg.repack();
                }
            }
            return;
        }
        let Some((_, seg)) = self.owned.iter_mut().find(|(q, _)| *q == p) else { return };
        match delta {
            Delta::Built { .. } => {}
            Delta::Delete { key, value } => seg.remove(&key, &value),
            Delta::Upsert { key, old, value, values } if values.is_empty() => {
                seg.apply(&key, old.as_ref(), Some(value))
            }
            Delta::Upsert { key, old, value, values } => {
                let refs: Vec<Option<&[u8]>> = values.iter().map(|v| v.as_deref()).collect();
                seg.apply_with_values(&key, old.as_ref(), Some(value), &refs);
            }
        }
    }

    fn send(&mut self, spec: &IndexSpec, p: u16, delta: Delta) {
        let to = partition_owner(spec.name(), p as usize, self.nshards);
        self.outbox.push((to, super::global_wire::encode(spec.name(), self.inc, p, &delta)));
    }
}

/// The row's entry: its index value and stored VALUES, or `None` when the
/// row is gone, not a hash, or excluded (a missing or uncoercible field).
pub(super) fn derive(
    store: &mut Store,
    spec: &IndexSpec,
    key: &[u8],
) -> Option<(IndexValue, Vec<Option<Vec<u8>>>)> {
    let names = spec.scalar_read_names();
    let w = spec.primary_width();
    let mut vals = store.peek_hash_fields(key, &names).ok()??;
    let value = spec.derive_scalar(&vals[..w])?;
    Some((value, vals.split_off(w)))
}

/// FNV-1a over the entry's encoded value and stored columns — computed
/// the same on the row's shard and, for `IDX.VERIFY`, on the owner.
pub(super) fn entry_hash<'a>(
    enc: &[u8],
    values: impl IntoIterator<Item = Option<&'a [u8]>>,
) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    };
    eat(enc);
    for v in values {
        // a tag first, so an absent column and an empty one differ
        match v {
            Some(b) => {
                eat(&[1]);
                eat(b);
            }
            None => eat(&[0]),
        }
    }
    h
}

/// The deltas this shard's global indexes queued since the last take
/// (`Commands::take_ext_out`). Nothing to walk when no index is global.
pub(crate) fn take_ext_out(ctx: &Ctx<'_>) -> Vec<(usize, Vec<u8>)> {
    let mut st = ctx.shard.indexes.borrow_mut();
    if !st.any_global {
        return Vec::new();
    }
    let mut out = Vec::new();
    for si in &mut st.idx {
        if let Some(g) = &mut si.global {
            out.append(&mut g.outbox);
        }
    }
    out
}

/// Apply a delta another shard's hook sent for a partition this shard owns
/// (`Commands::apply_ext`).
pub(crate) fn apply_ext(ctx: &Ctx<'_>, payload: &[u8]) {
    let Some((name, inc, p, delta)) = super::global_wire::decode(payload) else { return };
    let mut st = ctx.shard.indexes.borrow_mut();
    super::refresh(ctx, &mut st);
    let st = &mut *st;
    let role =
        st.idx.iter_mut().find(|si| si.spec.name() == name).and_then(|si| si.global.as_mut());
    if let Some(g) = role.filter(|g| g.inc == inc) {
        g.apply(p, delta);
        st.stats_dirty = true;
    }
}
