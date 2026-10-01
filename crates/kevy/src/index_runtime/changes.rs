//! Bringing this shard's indexes up to date with the rows written since the
//! last drain.
//!
//! The store records each written row's indexed fields as they were before
//! the first write since the last drain. With the row as it is now, that
//! names both the entry to drop and the entry to add, so a scalar index
//! keeps no map from key back to entry. The record covers every way a row
//! changes — commands, expiry, eviction, field TTLs, replication, Lua — so
//! a write this hook was never called for is still applied at the next
//! drain.

use kevy_index::{IndexSpec, IndexValue};
use kevy_store::{RowChange, RowWatch, Store};

use super::{ShardIndex, ShardIndexes};

/// The watch rules this shard's indexes need: per prefix, the fields
/// every scalar index on it reads.
pub(super) type Rules = Vec<(Vec<u8>, Vec<Vec<u8>>)>;

/// Where one index's read names sit in the installed rules: the rule, and
/// each name's position in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Slots {
    rule: usize,
    fields: Vec<usize>,
}

/// Composite columns an index reads at most, plus room for its values.
const MAX_READ: usize = 32;

fn wanted(idx: &[ShardIndex]) -> Rules {
    let mut rules: Rules = Vec::new();
    for si in idx {
        let p = si.spec.prefix();
        let at = match rules.iter().position(|(q, _)| q.as_slice() == p) {
            Some(i) => i,
            None => {
                rules.push((p.to_vec(), Vec::new()));
                rules.len() - 1
            }
        };
        if si.reads_old_rows() {
            for n in si.spec.scalar_read_names() {
                if !rules[at].1.iter().any(|f| f.as_slice() == n) {
                    rules[at].1.push(n.to_vec());
                }
            }
        }
    }
    rules.sort();
    rules
}

fn locate(rules: &Rules, spec: &IndexSpec) -> Option<Slots> {
    let rule = rules.iter().position(|(p, _)| p.as_slice() == spec.prefix())?;
    let fields = &rules[rule].1;
    let names = spec.scalar_read_names();
    let at = names.iter().map(|n| fields.iter().position(|f| f.as_slice() == *n));
    Some(Slots { rule, fields: at.collect::<Option<Vec<usize>>>()? })
}

/// Apply the store's record to every index, then make the watch match the
/// index list (only after the drain: the record's rules are the old
/// watch's).
pub(super) fn drain(store: &mut Store, st: &mut ShardIndexes) {
    if store.has_row_changes() {
        let changes = store.take_row_changes(std::mem::take(&mut st.spare));
        if changes.is_reset() {
            // the keyspace was wiped: start over, then add what came after
            super::reset_all(st);
            st.touched.wipe();
        }
        for c in changes.iter() {
            st.touched.push(c.key());
            for si in &mut st.idx {
                if c.key().starts_with(si.spec.prefix()) {
                    apply(store, si, &c);
                }
            }
        }
        st.spare = changes;
        st.stats_dirty = true;
    }
    if st.watch_gen != st.generation {
        let want = wanted(&st.idx);
        if want != st.installed {
            let w =
                want.iter().fold(RowWatch::new(), |w, (p, f)| w.with_prefix(p.clone(), f.clone()));
            store.set_row_watch(w);
            st.installed = want;
        }
        for si in &mut st.idx {
            si.slots = locate(&st.installed, &si.spec);
        }
        st.watch_gen = st.generation;
    }
}

/// The value and stored values `c` says the row held for `si`'s index,
/// `None` when it held no entry (or the index was not watched yet).
pub(super) fn old_entry(
    si: &ShardIndex,
    c: &RowChange<'_>,
) -> Option<(IndexValue, Vec<Option<Vec<u8>>>)> {
    let s = si.slots.as_ref()?;
    let w = si.spec.primary_width();
    let mut prim: [Option<&[u8]>; MAX_READ] = [None; MAX_READ];
    for (slot, &f) in prim.iter_mut().zip(&s.fields[..w]) {
        *slot = c.field(s.rule, f);
    }
    let v = si.spec.derive_scalar_refs(&prim[..w])?;
    // only a global index compares stored values (to skip an unchanged
    // entry); a local one locates the old entry by its value alone
    let vals = match si.global {
        Some(_) => s.fields[w..].iter().map(|&f| c.field(s.rule, f).map(<[u8]>::to_vec)).collect(),
        None => Vec::new(),
    };
    Some((v, vals))
}

fn apply(store: &mut Store, si: &mut ShardIndex, c: &RowChange<'_>) {
    let old = if si.reads_old_rows() { old_entry(si, c) } else { None };
    super::row_apply::apply_row(store, si, c.key(), old);
}
