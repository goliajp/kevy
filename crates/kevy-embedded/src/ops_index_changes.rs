//! Bringing a shard's indexes up to date with the rows written since the
//! last drain (server twin: `index_runtime::changes`).
//!
//! The store records each written row's indexed fields as they were
//! before the first write since the last drain; with the row as it is
//! now, that names both the entry to drop and the entry to add, so a
//! scalar index keeps no map from key back to entry. The record covers
//! every write the store sees — facade calls, transactions and their
//! rollbacks, expiry, eviction, replication frames — so a write no hook
//! was called for is applied at the next drain.

use kevy_index::{IndexSpec, IndexValue};
use kevy_store::{RowChange, RowChanges, RowWatch, Store};

use crate::ops_index::ShardSegs;

/// Watch rules: per prefix, the fields every scalar index on it reads.
pub(crate) type Rules = Vec<(Vec<u8>, Vec<Vec<u8>>)>;

/// The recorder's state a shard keeps between drains.
#[derive(Debug, Default)]
pub(crate) struct Drain {
    spare: RowChanges,
    installed: Rules,
    /// Where each scalar index's read names sit in `installed`.
    slots: Vec<Option<(usize, Vec<usize>)>>,
    /// Each index's old value for the change being applied, reused.
    olds: Vec<Option<IndexValue>>,
    /// The index-list version the installed rules were computed for.
    version: Option<u64>,
}

/// Composite columns an index reads at most, plus room for its values.
const MAX_READ: usize = 32;

fn wanted(ss: &ShardSegs) -> Rules {
    let mut rules: Rules = Vec::new();
    let mut add = |spec: &IndexSpec, scalar: bool| {
        let p = spec.prefix();
        let at = match rules.iter().position(|(q, _)| q.as_slice() == p) {
            Some(i) => i,
            None => {
                rules.push((p.to_vec(), Vec::new()));
                rules.len() - 1
            }
        };
        if scalar {
            for n in spec.scalar_read_names() {
                if !rules[at].1.iter().any(|f| f.as_slice() == n) {
                    rules[at].1.push(n.to_vec());
                }
            }
        }
    };
    ss.segs.iter().for_each(|(s, _)| add(s, true));
    #[cfg(feature = "text")]
    ss.text.iter().for_each(|(s, _)| add(s, false));
    #[cfg(feature = "vector")]
    ss.ann.iter().for_each(|(s, _)| add(s, false));
    ss.agg.iter().for_each(|(s, _)| add(s, false));
    rules.sort();
    rules
}

/// Where `spec`'s read names sit in `rules`: the rule and each field.
fn locate(rules: &Rules, spec: &IndexSpec) -> Option<(usize, Vec<usize>)> {
    let rule = rules.iter().position(|(p, _)| p.as_slice() == spec.prefix())?;
    let fields = &rules[rule].1;
    let names = spec.scalar_read_names();
    let at = names.iter().map(|n| fields.iter().position(|f| f.as_slice() == *n));
    Some((rule, at.collect::<Option<Vec<usize>>>()?))
}

fn slots_for(ss: &ShardSegs) -> Vec<Option<(usize, Vec<usize>)>> {
    ss.segs.iter().map(|(s, _)| locate(&ss.drain.installed, s)).collect()
}

/// The value `c` says the row was indexed under by `spec`.
fn old_value(
    spec: &IndexSpec,
    slots: Option<&(usize, Vec<usize>)>,
    c: &RowChange<'_>,
) -> Option<IndexValue> {
    let (rule, fields) = slots?;
    let w = spec.primary_width();
    let mut prim: [Option<&[u8]>; MAX_READ] = [None; MAX_READ];
    for (slot, &f) in prim.iter_mut().zip(&fields[..w]) {
        *slot = c.field(*rule, f);
    }
    spec.derive_scalar_refs(&prim[..w])
}

/// Apply the store's record to every index of this shard, then make the
/// watch match the index list (only after the drain: the record's rules
/// are the old watch's). Returns whether anything was applied.
pub(crate) fn drain(ss: &mut ShardSegs, store: &mut Store) -> bool {
    let mut applied = false;
    if store.has_row_changes() {
        let changes = store.take_row_changes(std::mem::take(&mut ss.drain.spare));
        if changes.is_reset() {
            // the keyspace was wiped: start over, then add what came after
            crate::ops_index_sync::reset_all_segs(ss);
        }
        if ss.drain.version != Some(ss.version) {
            // the index list moved since the slots were computed; the
            // record still follows the installed rules
            ss.drain.slots = slots_for(ss);
        }
        let mut olds = std::mem::take(&mut ss.drain.olds);
        for c in changes.iter() {
            olds.clear();
            let slots = &ss.drain.slots;
            olds.extend(
                ss.segs.iter().zip(slots).map(|((s, _), at)| old_value(s, at.as_ref(), &c)),
            );
            crate::ops_index_sync::apply_one_key(ss, store, c.key(), &olds);
        }
        ss.drain.olds = olds;
        ss.drain.spare = changes;
        applied = true;
    }
    if ss.drain.version != Some(ss.version) {
        let want = wanted(ss);
        if want != ss.drain.installed {
            let w =
                want.iter().fold(RowWatch::new(), |w, (p, f)| w.with_prefix(p.clone(), f.clone()));
            store.set_row_watch(w);
            ss.drain.installed = want;
        }
        ss.drain.slots = slots_for(ss);
        ss.drain.version = Some(ss.version);
    }
    applied
}
