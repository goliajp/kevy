//! Row values — a row's *own* stored field values, kept beside the
//! scalar postings.
//!
//! The segment maps **value → keys**: that is what a range scan needs.
//! `FILTER`, `SORT`, `DISTINCT` and `FACET` need the opposite direction
//! — **row → its own value** — which a `(value, key)` tree cannot answer
//! without a scan. Same idea (and the same inline-slot trade) as
//! kevy-text's doc values, keyed by row key rather than doc id because a
//! scalar segment has no id space of its own.
//!
//! Values are raw bytes: typing and coercion stay with the caller that
//! declared them ([`crate::ValueSpec`]), exactly as in the text twin.
//!
//! The channel exists only when the index declared `VALUES`; a segment
//! without the declaration holds `None` and never touches this module —
//! the `Option<Positions>` physical-bypass pattern (acceptance A5).

use std::collections::HashMap;

/// One stored value. Values up to 23 bytes live inline and cost no
/// allocation — the enum is 32 bytes either way (the same trade as
/// kevy-text's `Val`).
#[derive(Debug, Clone, Default, PartialEq)]
enum Val {
    /// The row has no value for this field. Not the same as an empty
    /// value: a predicate never passes on an absent one.
    #[default]
    Absent,
    Inline {
        len: u8,
        buf: [u8; 23],
    },
    Heap(Vec<u8>),
}

impl Val {
    fn store(v: &[u8]) -> Val {
        if v.len() <= 23 {
            let mut buf = [0u8; 23];
            buf[..v.len()].copy_from_slice(v);
            Val::Inline { len: v.len() as u8, buf }
        } else {
            Val::Heap(v.to_vec())
        }
    }

    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Val::Absent => None,
            Val::Inline { len, buf } => Some(&buf[..*len as usize]),
            Val::Heap(v) => Some(v),
        }
    }

    /// Heap bytes beyond the slot itself.
    fn heap(&self) -> u64 {
        match self {
            Val::Heap(v) => (v.len().max(1) as u64).next_multiple_of(16) + 16,
            _ => 0,
        }
    }
}

/// One row's contribution to the memory term: its key copy, the fixed
/// slot array, and any spilled (heap) values.
fn row_bytes(key_len: usize, slots: &[Val]) -> u64 {
    key_len as u64
        + slots.len() as u64 * std::mem::size_of::<Val>() as u64
        + slots.iter().map(Val::heap).sum::<u64>()
}

/// The stored-value side-channel: row key → its declared values.
#[derive(Debug)]
pub(crate) struct RowValues {
    /// How many value fields the index declares — the stride of a row's
    /// slot array.
    n: usize,
    rows: HashMap<Vec<u8>, Box<[Val]>>,
    /// Running Σ of [`row_bytes`], maintained on every `set`/`clear` so
    /// [`RowValues::approx_bytes`] is O(1). A full-map rescan per call
    /// was ~50 ms on the reactor at 10M rows — `Segment::stats()` (hence
    /// the tiering reserved-floor feed) reads it every 100 ms tick.
    heap: u64,
}

impl RowValues {
    pub(crate) fn new(n: usize) -> Self {
        Self { n, rows: HashMap::new(), heap: 0 }
    }

    /// Store `key`'s values. A shorter slice than the declared arity
    /// leaves the rest absent.
    pub(crate) fn set(&mut self, key: &[u8], values: &[Option<&[u8]>]) {
        let mut slots = vec![Val::Absent; self.n].into_boxed_slice();
        for (f, slot) in slots.iter_mut().enumerate() {
            if let Some(v) = values.get(f).copied().flatten() {
                *slot = Val::store(v);
            }
        }
        self.heap += row_bytes(key.len(), &slots);
        if let Some(old) = self.rows.insert(key.to_vec(), slots) {
            self.heap = self.heap.saturating_sub(row_bytes(key.len(), &old));
        }
    }

    /// Forget `key`'s values — the withdrawal counterpart of
    /// [`RowValues::set`].
    pub(crate) fn clear(&mut self, key: &[u8]) {
        if let Some(old) = self.rows.remove(key) {
            self.heap = self.heap.saturating_sub(row_bytes(key.len(), &old));
        }
    }

    /// `key`'s value for `field`, or `None` when the row has none.
    pub(crate) fn get(&self, key: &[u8], field: usize) -> Option<&[u8]> {
        self.rows.get(key)?.get(field)?.bytes()
    }

    /// Approximate heap bytes — the stored-value term of the memory
    /// formula (slot arrays + key copies + spilled values). O(1): the
    /// running total is maintained by `set`/`clear`.
    pub(crate) fn approx_bytes(&self) -> u64 {
        self.heap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_slot_is_thirty_two_bytes_either_way() {
        assert_eq!(std::mem::size_of::<Val>(), 32);
    }

    #[test]
    fn short_values_stay_inline_long_ones_spill() {
        assert!(matches!(Val::store(b"42"), Val::Inline { .. }));
        assert!(matches!(Val::store(&[b'x'; 24]), Val::Heap(_)));
        assert_eq!(Val::store(b"").bytes(), Some(&b""[..]), "empty is a value");
        assert_eq!(Val::Absent.bytes(), None, "absent is not");
    }

    #[test]
    fn set_get_clear_across_the_stride() {
        let mut rv = RowValues::new(2);
        rv.set(b"u:1", &[Some(b"active"), Some(&[b'y'; 40])]);
        assert_eq!(rv.get(b"u:1", 0), Some(&b"active"[..]));
        assert_eq!(rv.get(b"u:1", 1), Some(&[b'y'; 40][..]), "long values round-trip");
        assert_eq!(rv.get(b"u:1", 2), None, "past the declared arity");
        assert_eq!(rv.get(b"u:2", 0), None, "a row that was never set");

        // A short slice leaves the rest absent; a re-set replaces whole.
        rv.set(b"u:1", &[Some(b"gone")]);
        assert_eq!(rv.get(b"u:1", 1), None, "not carried over from the last write");
        rv.clear(b"u:1");
        assert_eq!(rv.get(b"u:1", 0), None);
    }

    #[test]
    fn approx_bytes_counts_the_spill() {
        let mut rv = RowValues::new(1);
        rv.set(b"k", &[Some(b"short")]);
        let inline_only = rv.approx_bytes();
        rv.set(b"k", &[Some(&[b'z'; 200])]);
        assert!(rv.approx_bytes() > inline_only, "a spilled value costs heap");
    }

    #[test]
    fn incremental_heap_matches_a_full_rescan() {
        // The O(1) running total must equal a from-scratch sum after any
        // mix of inserts, overwrites (inline↔spill), and clears — the
        // property that lets Segment::stats() skip the per-tick rescan.
        let recompute = |rv: &RowValues| -> u64 {
            rv.rows
                .iter()
                .map(|(k, slots)| row_bytes(k.len(), slots))
                .sum()
        };
        let mut rv = RowValues::new(2);
        rv.set(b"a", &[Some(b"x"), Some(&[b'z'; 40])]);
        rv.set(b"bb", &[Some(&[b'y'; 100]), None]);
        rv.set(b"a", &[Some(b"short"), None]); // overwrite: spill → inline, arity shrinks
        rv.set(b"ccc", &[Some(b"1"), Some(b"2")]);
        rv.clear(b"bb");
        rv.clear(b"missing"); // no-op must not perturb the total
        assert_eq!(rv.approx_bytes(), recompute(&rv), "counter drifted from truth");
        rv.clear(b"a");
        rv.clear(b"ccc");
        assert_eq!(rv.approx_bytes(), recompute(&rv));
        assert_eq!(rv.approx_bytes(), 0, "an empty side-channel costs nothing");
    }
}
