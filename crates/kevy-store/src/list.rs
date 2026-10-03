//! `Store` list write commands. Reads live in `list_read.rs`.
//!
//! Three encodings, promoted in order of size: `SmallListInline`
//! (≤8 tiny elements, in the Value body) → `List(Arc<VecDeque>)` (flat
//! heap) → `SegList` (per-segment COW past
//! [`crate::list_seg::SEG_PROMOTE`] elements — a write under a live
//! snapshot view clones one segment, not the whole value).

use crate::list_seg::{SEG_PROMOTE, SegListData};
use crate::small_list::{self, PushResult, SmallListData};
use crate::util::{norm_index, range_bounds};
use crate::value::{ListData, SmallBytes, Value, list_item_weight};
use crate::{Entry, InsertPosition, Store, StoreError};
use alloc::sync::Arc;

/// Push into an inline list, promoting it to the heap encoding if the value
/// does not fit. Returns whether the promotion happened, which is the only
/// thing the caller does differently.
///
/// Split from `list_push_at` for the 50-line rule. `slot` must be a
/// `Value::SmallListInline`; the caller checks, and the `else` here pushes
/// nothing rather than asserting, because a wrong caller should not be a
/// panic in the write path.
fn push_inline(slot: &mut Value, v: &[u8], front: bool) -> bool {
    let Value::SmallListInline(s) = slot else { return false };
    let push = if front { s.try_push_front(v) } else { s.try_push_back(v) };
    match push {
        PushResult::Pushed => false,
        PushResult::NoRoom => {
            let mut promoted = small_list::promote(s);
            if front {
                promoted.push_front(v.to_vec());
            } else {
                promoted.push_back(v.to_vec());
            }
            *slot = Value::List(Arc::new(promoted));
            true
        }
    }
}

impl Store {
    // ---- lists ---------------------------------------------------------

    /// Borrow the key's flat list mutably; promote inline → heap if
    /// needed. `create == true` materialises a fresh empty heap list
    /// when the key is missing (the `lset/lpop/rpop/lrem/ltrim` legacy
    /// paths). Callers dispatch the `SegList` encoding BEFORE calling
    /// this — a seg-encoded key never reaches the `make_mut` here.
    pub(crate) fn list_mut(
        &mut self,
        key: &[u8],
        create: bool,
    ) -> Result<Option<&mut ListData>, StoreError> {
        if self.live_entry_mut(key).is_none() {
            if !create {
                return Ok(None);
            }
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::List(Arc::default()), None),
            );
        }
        // A.8: see hash.rs::hash_mut — promote out-of-scope, then
        // re-borrow as the heap variant.
        let is_inline =
            matches!(self.map.get(key).map(|e| &e.value), Some(Value::SmallListInline(_)));
        if is_inline {
            let promoted = {
                let e = self.map.get(key).expect("present");
                if let Value::SmallListInline(s) = &e.value {
                    small_list::promote(s)
                } else {
                    unreachable!()
                }
            };
            self.map.get_mut(key).expect("present").value = Value::List(Arc::new(promoted));
            self.reweigh_entry(key);
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::List(l) => Ok(Some(Arc::make_mut(l))),
            _ => Err(StoreError::WrongType),
        }
    }

    /// Whether `key` currently holds the `SegList` encoding (after lazy
    /// expiry). The write ops branch on this before the flat path.
    pub(crate) fn is_seglist(&mut self, key: &[u8]) -> bool {
        matches!(self.live_entry_mut(key).map(|e| &e.value), Some(Value::SegList(_)))
    }

    /// Borrow the seg-encoded list mutably. Caller has checked
    /// [`Self::is_seglist`]; the outer `make_mut` here is the cheap
    /// pointer-array clone (the per-segment clones happen inside
    /// `SegListData`'s ops, only on touched segments).
    pub(crate) fn seglist_mut(&mut self, key: &[u8]) -> &mut SegListData {
        match &mut self.map.get_mut(key).expect("is_seglist checked").value {
            Value::SegList(l) => Arc::make_mut(l),
            _ => unreachable!("is_seglist checked"),
        }
    }

    /// Remove `key` if it now holds an empty list (any encoding).
    pub(crate) fn drop_if_empty_list(&mut self, key: &[u8]) {
        let empty = match self.map.get(key).map(|e| &e.value) {
            Some(Value::List(l)) => l.is_empty(),
            Some(Value::SegList(l)) => l.is_empty(),
            Some(Value::SmallListInline(l)) => l.is_empty(),
            _ => false,
        };
        if empty {
            self.remove_emptied(key);
        }
    }

    /// Return the list's length (any encoding). Used by the public
    /// push functions to compute "new length" after spending entries.
    fn list_len(&self, key: &[u8]) -> usize {
        match self.map.get(key).map(|e| &e.value) {
            Some(Value::List(l)) => l.len(),
            Some(Value::SegList(l)) => l.len(),
            Some(Value::SmallListInline(l)) => l.len(),
            _ => 0,
        }
    }

    /// `LPUSH` — prepend each value in turn; returns the new length.
    pub fn lpush(&mut self, key: &[u8], values: &[&[u8]]) -> Result<usize, StoreError> {
        self.list_push(key, values, true, true)
    }

    /// `RPUSH` — append each value; returns the new length.
    pub fn rpush(&mut self, key: &[u8], values: &[&[u8]]) -> Result<usize, StoreError> {
        self.list_push(key, values, false, true)
    }

    /// `LPUSHX` — [`Self::lpush`] onto a list that exists; `0` otherwise.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// assert_eq!(s.lpushx(b"q", &[b"a".as_slice()])?, 0);
    /// s.rpush(b"q", &[b"a".as_slice()])?;
    /// assert_eq!(s.lpushx(b"q", &[b"b".as_slice()])?, 2);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn lpushx(&mut self, key: &[u8], values: &[&[u8]]) -> Result<usize, StoreError> {
        self.list_push(key, values, true, false)
    }

    /// `RPUSHX` — [`Self::rpush`] onto a list that exists; `0` otherwise.
    pub fn rpushx(&mut self, key: &[u8], values: &[&[u8]]) -> Result<usize, StoreError> {
        self.list_push(key, values, false, false)
    }

    /// The push loop: one probe for the whole command, every value then
    /// reaching the list through its slot, which no push can move.
    fn list_push(
        &mut self,
        key: &[u8],
        values: &[&[u8]],
        front: bool,
        create: bool,
    ) -> Result<usize, StoreError> {
        let Some((first, rest)) = values.split_first() else {
            return Ok(self.list_len(key));
        };
        let (slot, todo) = match self.live_slot(key) {
            Some(slot) => (slot, values),
            None if create => (self.list_push_create(key, first), rest),
            None => return Ok(0),
        };
        let mut delta: i64 = 0;
        for v in todo {
            delta += self.list_push_at(key, slot, v, front)?;
        }
        self.account_delta_at(Some(slot), delta);
        Ok(match self.map.slot(slot).map(|(_, e)| &e.value) {
            Some(Value::List(l)) => l.len(),
            Some(Value::SegList(l)) => l.len(),
            Some(Value::SmallListInline(l)) => l.len(),
            _ => 0,
        })
    }

    /// Push one element into the entry at `at`, applying the
    /// encoding-switch. Returns the per-element weight delta (zero for
    /// inline / reweighed cases, list_item_weight for heap). A value of
    /// another type is refused before anything changes.
    fn list_push_at(
        &mut self,
        key: &[u8],
        at: usize,
        v: &[u8],
        front: bool,
    ) -> Result<i64, StoreError> {
        let slot = &mut self.map.entry_at_mut(key, at).expect("the slot live_slot found").value;
        if matches!(slot, Value::SmallListInline(_)) {
            // Promotion reweighs from scratch, so the caller's delta for
            // THIS pair is 0 either way — the new weight already has it.
            if push_inline(slot, v, front) {
                self.reweigh_at(at, None);
            }
            return Ok(0);
        }
        let reweigh = match slot {
            Value::List(l) if l.len() >= SEG_PROMOTE => {
                promote_flat_to_seg(slot, v, front);
                true
            }
            Value::List(l) => {
                let l = Arc::make_mut(l);
                let cap = l.capacity();
                if front {
                    l.push_front(v.to_vec())
                } else {
                    l.push_back(v.to_vec())
                }
                return Ok(flat_delta(l, cap, v.len() as i64));
            }
            Value::SegList(l) => {
                let l = Arc::make_mut(l);
                if front {
                    l.push_front(v.to_vec())
                } else {
                    l.push_back(v.to_vec())
                }
                return Ok(list_item_weight(v.len()) as i64);
            }
            _ => return Err(StoreError::WrongType),
        };
        if reweigh {
            self.reweigh_at(at, None);
        }
        Ok(0)
    }

    /// Create a fresh entry holding one element, inline if it fits, else
    /// heap; its slot.
    fn list_push_create(&mut self, key: &[u8], v: &[u8]) -> usize {
        let value = match SmallListData::with_one(v) {
            Some(inline) => Value::SmallListInline(inline),
            None => {
                let mut d = alloc::collections::VecDeque::with_capacity(1);
                d.push_back(v.to_vec());
                Value::List(Arc::new(d))
            }
        };
        self.insert_entry_at(SmallBytes::from_slice(key), Entry::new(value, None)).0
    }

    /// `LPOP`/`RPOP` shared body — pop up to `count` from one end.
    /// Force-promote an inline list at `key` to its heap variant
    /// (no-op if already heap or absent). Used by mutating paths that
    /// only support the heap representations (pop/lrem/lset/ltrim).
    fn promote_list_inline_to_heap(&mut self, key: &[u8]) {
        let needs = matches!(self.map.get(key).map(|e| &e.value), Some(Value::SmallListInline(_)));
        if !needs {
            return;
        }
        let Some(e) = self.map.get_mut_quiet(key) else { return };
        if let Value::SmallListInline(s) = &e.value {
            let promoted = small_list::promote(s);
            e.value = Value::List(Arc::new(promoted));
        }
        self.reweigh_entry(key);
    }

    /// `LSET` — errors with `NoSuchKey` / `OutOfRange` like Redis.
    pub fn lset(&mut self, key: &[u8], idx: i64, val: &[u8]) -> Result<(), StoreError> {
        self.promote_list_inline_to_heap(key);
        let delta = if self.is_seglist(key) {
            let l = self.seglist_mut(key);
            let i = norm_index(idx, l.len()).ok_or(StoreError::OutOfRange)?;
            let old = l.set(i, val.to_vec());
            val.len() as i64 - old.len() as i64
        } else {
            let l = self.list_mut(key, false)?.ok_or(StoreError::NoSuchKey)?;
            let i = norm_index(idx, l.len()).ok_or(StoreError::OutOfRange)?;
            let old = l[i].capacity() as i64;
            l[i] = val.to_vec();
            val.len() as i64 - old
        };
        self.account_delta(key, delta);
        Ok(())
    }

    /// `LINSERT key BEFORE|AFTER pivot value` — insert `value`
    /// before/after the first occurrence of `pivot` in the list at
    /// `key`. Returns:
    /// - new list length on success (`>= 1`);
    /// - `0` when `key` does not exist;
    /// - `-1` when `pivot` was not found in the list.
    ///
    /// Matches Redis semantics.
    pub fn linsert(
        &mut self,
        key: &[u8],
        position: InsertPosition,
        pivot: &[u8],
        val: &[u8],
    ) -> Result<i64, StoreError> {
        let beside = |idx: usize| match position {
            InsertPosition::Before => idx,
            InsertPosition::After => idx + 1,
        };
        self.promote_list_inline_to_heap(key);
        let (result, delta) = if self.is_seglist(key) {
            let l = self.seglist_mut(key);
            let Some(idx) = l.position(pivot) else {
                return Ok(-1);
            };
            let insert_at = beside(idx);
            l.insert(insert_at, val.to_vec());
            (l.len() as i64, list_item_weight(val.len()) as i64)
        } else {
            match self.list_mut(key, false)? {
                None => return Ok(0),
                Some(l) => {
                    let Some(idx) = l.iter().position(|v| v.as_slice() == pivot) else {
                        return Ok(-1);
                    };
                    let insert_at = beside(idx);
                    let cap = l.capacity();
                    l.insert(insert_at, val.to_vec());
                    (l.len() as i64, flat_delta(l, cap, val.len() as i64))
                }
            }
        };
        self.account_delta(key, delta);
        Ok(result)
    }

    /// `LREM` — remove `count` occurrences (>0 head, <0 tail, 0 all).
    pub fn lrem(&mut self, key: &[u8], count: i64, val: &[u8]) -> Result<usize, StoreError> {
        self.promote_list_inline_to_heap(key);
        let (removed, delta) = if self.is_seglist(key) {
            self.seglist_mut(key).remove_occurrences(val, count)
        } else {
            match self.list_mut(key, false)? {
                None => (0, 0),
                Some(l) => flat_lrem(l, count, val),
            }
        };
        self.account_delta(key, delta);
        self.drop_if_empty_list(key);
        Ok(removed)
    }

    /// `LTRIM` — keep only `[start, stop]` (deleting emptied key).
    pub fn ltrim(&mut self, key: &[u8], start: i64, stop: i64) -> Result<(), StoreError> {
        self.promote_list_inline_to_heap(key);
        let delta = if self.is_seglist(key) {
            let l = self.seglist_mut(key);
            match range_bounds(start, stop, l.len()) {
                None => l.clear(),
                Some((s, e)) => l.trim_to(s, e),
            }
        } else if let Some(l) = self.list_mut(key, false)? {
            match range_bounds(start, stop, l.len()) {
                // removing items keeps the deque's slots: only the items' bytes go
                None => {
                    let d = -(l.iter().map(|v| v.capacity() as i64).sum::<i64>());
                    l.clear();
                    d
                }
                Some((s, e)) => {
                    let mut d: i64 = 0;
                    for v in l.iter().skip(e + 1) {
                        d -= v.capacity() as i64;
                    }
                    l.drain(e + 1..);
                    for v in l.iter().take(s) {
                        d -= v.capacity() as i64;
                    }
                    l.drain(..s);
                    d
                }
            }
        } else {
            0
        };
        self.account_delta(key, delta);
        self.drop_if_empty_list(key);
        Ok(())
    }
}

/// Flat list at the promotion threshold: re-encode as segments, then
/// push. One-time O(SEG_PROMOTE) move (or clone, if a snapshot view
/// pins the flat Arc right now). Caller reweighs the entry.
fn promote_flat_to_seg(slot: &mut Value, v: &[u8], front: bool) {
    let flat = match core::mem::replace(slot, Value::Int(0)) {
        Value::List(a) => Arc::try_unwrap(a).unwrap_or_else(|a| (*a).clone()),
        _ => unreachable!("matched List"),
    };
    let mut seg = SegListData::from_flat(flat);
    if front {
        seg.push_front(v.to_vec())
    } else {
        seg.push_back(v.to_vec())
    }
    *slot = Value::SegList(Arc::new(seg));
}

/// The flat-list LREM walk (unchanged semantics; hoisted out of the
/// method so the seg/flat dispatch stays within the fn-LOC cap).
/// A flat list's weight change: `bytes` of items in or out, plus its
/// deque's slots, which follow its capacity (they stay allocated as items
/// leave, and grow in steps as items arrive).
pub(crate) fn flat_delta(l: &ListData, cap_before: usize, bytes: i64) -> i64 {
    (l.capacity() as i64 - cap_before as i64) * crate::value::LIST_SLOT_BYTES as i64 + bytes
}

fn flat_lrem(l: &mut ListData, count: i64, val: &[u8]) -> (usize, i64) {
    let mut r = 0usize;
    let mut d: i64 = 0;
    if count >= 0 {
        let limit = if count == 0 { usize::MAX } else { count as usize };
        let mut i = 0;
        while i < l.len() {
            if r < limit && l[i] == val {
                d -= l[i].capacity() as i64;
                l.remove(i);
                r += 1;
            } else {
                i += 1;
            }
        }
    } else {
        let limit = (-count) as usize;
        let mut i = l.len();
        while i > 0 {
            i -= 1;
            if r < limit && l[i] == val {
                d -= l[i].capacity() as i64;
                l.remove(i);
                r += 1;
            }
        }
    }
    (r, d)
}
