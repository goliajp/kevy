//! Moving hot values out of sparse allocator spans.
//!
//! Demotion frees rows wherever they happen to sit, and an allocator can
//! hand a page back only once nothing on it is live, so a tiered store
//! that demoted a fifth of its rows keeps most of the pages they were on.
//! The allocator knows which of its spans are sparser than their class
//! (it answers through a [`DefragHint`]); only the store knows who owns a
//! value, so the store does the move: it walks its table, copies each hot
//! value the hint names into a fresh allocation — which the allocator
//! places in a denser span — and drops the original. What a span loses
//! that way it gives back whole at the next reclaim.
//!
//! Without a hint (the default, and always under the system allocator)
//! nothing here runs. The walk covers the values whose bytes dominate a
//! row store: strings, hashes, packed rows. A value shared through an
//! `Arc` (a snapshot or a read in flight holds it) stays where it is:
//! copying it would hold both.

use crate::Store;
#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::{SmallBytes, Value};
use alloc::sync::Arc;

/// Whether the allocation at `ptr`, made with `(size, align)`, would move
/// to a denser span if copied. The store only asks about allocations its
/// values own, with the layout they were made with; a hint must still
/// answer safely for any address (`kevy_alloc::global::should_move` does).
///
/// ```
/// use kevy_store::{DefragHint, Store};
/// // a hint that never asks for a move: installing it changes nothing
/// let never: DefragHint = |_, _, _| false;
/// let mut s = Store::new();
/// s.set_defrag_hint(Some(never));
/// assert_eq!(s.defrag_step(64).moved, 0);
/// ```
pub type DefragHint = fn(ptr: *const u8, size: usize, align: usize) -> bool;

/// The store's side of a defrag pass: the hint, and where the walk is.
#[derive(Debug, Default)]
pub(crate) struct DefragState {
    hint: Option<DefragHint>,
    hand: usize,
}

/// What one [`Store::defrag_step`] did.
///
/// ```
/// use kevy_store::{DefragStep, Store};
/// let mut s = Store::new();
/// let step: DefragStep = s.defrag_step(64);
/// assert_eq!(step.moved, 0, "no hint, no move");
/// assert!(step.lap_done);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct DefragStep {
    /// Values copied to a fresh allocation.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// // with every allocation hinted to move, the one heap-held value is copied
    /// let mut s = Store::new();
    /// s.set_slice(b"k", &[7u8; 64], None, SetCondition::Always);
    /// s.set_defrag_hint(Some(|_, _, _| true));
    /// let mut moved = 0;
    /// while !{ let step = s.defrag_step(64); moved += step.moved; step.lap_done } {}
    /// assert_eq!(moved, 1);
    /// ```
    pub moved: usize,
    /// The walk reached the end of the table and starts over next time.
    ///
    /// ```
    /// use kevy_store::Store;
    /// // an empty table is walked in one step
    /// let mut s = Store::new();
    /// assert!(s.defrag_step(1).lap_done);
    /// ```
    pub lap_done: bool,
}

/// Whether a heap-held byte string sits where the hint wants it gone. A
/// heap `SmallBytes` owns a buffer of `heap_bytes()` capacity, alignment 1.
fn bytes_hinted(hint: DefragHint, s: &SmallBytes) -> bool {
    let size = s.heap_bytes();
    size != 0 && hint(s.as_slice().as_ptr(), size, 1)
}

/// Whether the block an `Arc` keeps its counts and value in should move.
/// The block is laid out as std lays it out — the two counts, then the
/// value — and a hint answers safely for any address, so a layout that
/// ever differed would only make this answer no.
fn arc_hinted<T>(hint: DefragHint, a: &Arc<T>) -> bool {
    let counts = core::alloc::Layout::new::<[usize; 2]>();
    let Ok((block, offset)) = counts.extend(core::alloc::Layout::new::<T>()) else {
        return false;
    };
    let block = block.pad_to_align();
    let start = Arc::as_ptr(a).cast::<u8>().wrapping_sub(offset);
    hint(start, block.size(), block.align())
}

/// Whether any of `v`'s dominant allocations should move, for values the
/// walk knows how to copy and nobody shares.
fn wants_move(hint: DefragHint, v: &Value) -> bool {
    match v {
        Value::Str(s) => bytes_hinted(hint, s),
        Value::ArcBulk(b) => {
            Arc::strong_count(b) == 1 && !b.is_empty() && hint(b.as_ptr(), b.len(), 1)
        }
        Value::Hash(h) => {
            Arc::strong_count(h) == 1
                && (arc_hinted(hint, h)
                    || h.table_allocation().is_some_and(|(p, l)| hint(p, l.size(), l.align()))
                    || h.iter().any(|(f, x)| bytes_hinted(hint, f) || bytes_hinted(hint, x)))
        }
        Value::PackedRow(r) => {
            let b = r.buffer();
            !b.is_empty() && hint(b.as_ptr(), b.len(), 1)
        }
        _ => false,
    }
}

/// Give `v` fresh allocations with the same contents.
fn rehome(v: &mut Value) {
    match v {
        Value::Str(s) => *s = s.clone(),
        Value::ArcBulk(b) => *b = Arc::new(Box::from(&b[..])),
        Value::Hash(h) => *h = Arc::new((**h).clone()),
        Value::PackedRow(r) => *r = r.clone(),
        _ => {}
    }
}

impl Store {
    /// Install the allocator's defrag hint, or remove it with `None`. Only
    /// the process that installed a hint-capable global allocator can give
    /// one; without it [`Self::defrag_step`] moves nothing.
    ///
    /// ```
    /// use kevy_store::Store;
    /// fn never(_: *const u8, _: usize, _: usize) -> bool { false }
    /// let mut s = Store::new();
    /// s.set_defrag_hint(Some(never));
    /// s.set_slice(b"k", &[b'v'; 100], None, kevy_store::SetCondition::Always);
    /// assert_eq!(s.defrag_step(64).moved, 0);
    /// ```
    pub fn set_defrag_hint(&mut self, hint: Option<DefragHint>) {
        self.defrag.hint = hint;
    }

    /// Walk `buckets` buckets of the table from where the last step
    /// stopped, and copy every hot value the hint names to a fresh
    /// allocation. Contents, TTLs, access clocks and cold keys are
    /// untouched; `used_memory` follows each value's new footprint.
    ///
    /// ```
    /// use kevy_store::{SetCondition, Store};
    /// // a hint that names every allocation: every hot value is copied
    /// fn always(_: *const u8, _: usize, _: usize) -> bool { true }
    /// let mut s = Store::new();
    /// s.set_slice(b"k", &[b'v'; 100], None, SetCondition::Always);
    /// s.set_defrag_hint(Some(always));
    /// let mut moved = 0;
    /// while !{ let step = s.defrag_step(64); moved += step.moved; step.lap_done } {}
    /// assert_eq!(moved, 1);
    /// assert_eq!(s.get(b"k")?.as_deref(), Some(&[b'v'; 100][..]));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn defrag_step(&mut self, buckets: usize) -> DefragStep {
        let cap = self.map.capacity();
        let Some(hint) = self.defrag.hint.filter(|_| cap != 0) else {
            return DefragStep { moved: 0, lap_done: true };
        };
        let start = if self.defrag.hand < cap { self.defrag.hand } else { 0 };
        let mut picked: Vec<Vec<u8>> = Vec::new();
        let next = self.map.scan_buckets(start, buckets.max(1), |k, e| {
            if wants_move(hint, &e.value) {
                picked.push(k.to_vec());
            }
        });
        for k in &picked {
            if let Some(e) = self.map.get_mut_quiet(k) {
                rehome(&mut e.value);
            }
            self.reweigh_entry(k);
        }
        let lap_done = next >= cap;
        self.defrag.hand = if lap_done { 0 } else { next };
        DefragStep { moved: picked.len(), lap_done }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SetCondition;
    use crate::key_heap_bytes_for;
    use core::time::Duration;

    fn always(_: *const u8, _: usize, _: usize) -> bool {
        true
    }

    #[test]
    fn a_full_lap_moves_every_unshared_value_and_keeps_the_books() {
        let mut s = Store::new();
        s.set_slice(b"str", &[b's'; 300], Some(Duration::from_secs(600)), SetCondition::Always);
        s.set_slice(b"small", b"inline", None, SetCondition::Always);
        let long: Vec<u8> = vec![b'v'; 900];
        for i in 0..200u32 {
            let k = format!("row:{i}");
            s.hset(k.as_bytes(), &[(b"pad".as_slice(), long.as_slice()), (b"n", b"1")]).unwrap();
        }
        // a snapshot holding row:0's hash
        let shared = match &s.map.get(b"row:0".as_slice()).unwrap().value {
            Value::Hash(h) => Arc::clone(h),
            other => panic!("row:0 is {other:?}"),
        };
        let before: Vec<(Vec<u8>, Vec<Vec<u8>>)> = (0..200u32)
            .map(|i| {
                (format!("row:{i}").into_bytes(), s.hgetall(format!("row:{i}").as_bytes()).unwrap())
            })
            .collect();
        s.set_defrag_hint(Some(always));
        let mut moved = 0;
        loop {
            let step = s.defrag_step(64);
            moved += step.moved;
            if step.lap_done {
                break;
            }
        }
        assert_eq!(moved, 200, "199 unshared rows and the long string; the shared row stays");
        drop(shared);
        for (k, fields) in before {
            assert_eq!(s.hgetall(&k).unwrap(), fields);
        }
        assert_eq!(s.get(b"str").unwrap().as_deref(), Some(&[b's'; 300][..]));
        assert!(s.pttl(b"str") > 0, "a deadline survives the move");
        let mut sum = s.keyspace_bytes;
        s.map.scan_buckets(0, usize::MAX, |k, e| {
            assert_eq!(e.weight(), key_heap_bytes_for(k.as_slice()) + e.value.weight(), "{k:?}");
            sum += e.weight();
        });
        assert_eq!(s.used_memory(), sum);
    }

    fn never(_: *const u8, _: usize, _: usize) -> bool {
        false
    }

    /// Names byte buffers only: no `Arc` block and no hash table.
    fn bytes_only(_: *const u8, _: usize, align: usize) -> bool {
        align == 1
    }

    fn lap(s: &mut Store) -> usize {
        let mut moved = 0;
        loop {
            let step = s.defrag_step(16);
            moved += step.moved;
            if step.lap_done {
                return moved;
            }
        }
    }

    /// One value of every kind the walk looks at, keyed by what it is.
    fn mixed() -> Store {
        let mut s = Store::new();
        s.set_slice(b"str", &[b's'; 40], None, SetCondition::Always);
        s.set_slice(b"inline", b"short", None, SetCondition::Always);
        s.set_slice(b"int", b"42", None, SetCondition::Always);
        s.rpush(b"list", &[b"x"]).unwrap();
        s.hset(b"spilled", &[(b"f".as_slice(), [b'v'; 100].as_slice())]).unwrap();
        s.hset(b"flat", &[(b"f".as_slice(), b"v".as_slice())]).unwrap();
        let wide: Vec<(Vec<u8>, &[u8])> =
            (0..200).map(|i| (format!("f{i}").into_bytes(), b"v".as_slice())).collect();
        let pairs: Vec<(&[u8], &[u8])> = wide.iter().map(|(f, v)| (&f[..], *v)).collect();
        s.hset(b"wide", &pairs).unwrap();
        s.hset(b"row", &[(b"id".as_slice(), b"7".as_slice())]).unwrap();
        let table: crate::packed_row::ColumnNames = vec![b"id".to_vec()].into();
        s.pack_row(b"row", &table);
        assert!(s.is_packed(b"row"));
        s
    }

    #[test]
    fn without_a_hint_or_a_table_a_step_does_nothing() {
        let mut s = mixed();
        assert_eq!(s.defrag_step(64), DefragStep { moved: 0, lap_done: true });
        let mut empty = Store::new();
        empty.set_defrag_hint(Some(always));
        assert_eq!(empty.defrag_step(64), DefragStep { moved: 0, lap_done: true });
    }

    #[test]
    fn a_hint_that_names_nothing_moves_nothing() {
        let mut s = mixed();
        let before = s.used_memory();
        s.set_defrag_hint(Some(never));
        assert_eq!(lap(&mut s), 0);
        assert_eq!(s.used_memory(), before);
    }

    #[test]
    fn a_hint_on_byte_buffers_moves_the_values_that_hold_one() {
        let mut s = mixed();
        s.set_defrag_hint(Some(bytes_only));
        // the heap string, the hash with a spilled value, the packed row
        assert_eq!(lap(&mut s), 3);
        assert_eq!(s.get(b"str").unwrap().as_deref(), Some(&[b's'; 40][..]));
        assert_eq!(s.hget(b"spilled", b"f").unwrap(), Some(&[b'v'; 100][..]));
        assert_eq!(s.hget(b"row", b"id").unwrap(), Some(&b"7"[..]));
        assert!(s.is_packed(b"row"), "a moved row keeps its form");
        assert_eq!(s.hlen(b"wide").unwrap(), 200);
    }

    #[test]
    fn a_walk_whose_table_shrank_starts_over() {
        let mut s = Store::new();
        for i in 0..300u32 {
            s.set_slice(format!("k{i}").as_bytes(), &[b'v'; 40], None, SetCondition::Always);
        }
        s.set_defrag_hint(Some(always));
        assert!(!s.defrag_step(100).lap_done);
        drop(s.detach_entries());
        s.set_slice(b"k", &[b'v'; 40], None, SetCondition::Always);
        assert_eq!(s.defrag_step(64), DefragStep { moved: 1, lap_done: true });
    }
}
