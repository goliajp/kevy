//! `Store` list commands.

use crate::small_list::{self, PushResult, SmallListData};
use crate::util::{norm_index, range_bounds};
use crate::value::{ListData, SmallBytes, Value, list_item_weight};
use crate::{Entry, Store, StoreError};
use std::sync::Arc;

impl Store {
    // ---- lists ---------------------------------------------------------

    /// Borrow the key's list mutably; promote inline → heap if needed.
    /// `create == true` materialises a fresh empty heap list when the
    /// key is missing (the `lset/lpop/rpop/lrem/ltrim` legacy paths).
    fn list_mut(&mut self, key: &[u8], create: bool) -> Result<Option<&mut ListData>, StoreError> {
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
        let is_inline = matches!(
            self.map.get(key).map(|e| &e.value),
            Some(Value::SmallListInline(_))
        );
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

    /// A.8: read the key's list slot for LPUSH/RPUSH. `WrongType` on
    /// non-list. Returns `None` when key is absent — caller creates.
    fn list_value_for_push(&mut self, key: &[u8]) -> Result<Option<&mut Value>, StoreError> {
        match self.live_entry_mut(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::List(_) | Value::SmallListInline(_) => Ok(Some(&mut e.value)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Remove `key` if it now holds an empty list (either encoding).
    fn drop_if_empty_list(&mut self, key: &[u8]) {
        let empty = match self.map.get(key).map(|e| &e.value) {
            Some(Value::List(l)) => l.is_empty(),
            Some(Value::SmallListInline(l)) => l.is_empty(),
            _ => false,
        };
        if empty {
            self.remove_entry(key);
        }
    }

    /// Return the list's length (either encoding). Used by the public
    /// push functions to compute "new length" after spending entries.
    fn list_len(&self, key: &[u8]) -> usize {
        match self.map.get(key).map(|e| &e.value) {
            Some(Value::List(l)) => l.len(),
            Some(Value::SmallListInline(l)) => l.len(),
            _ => 0,
        }
    }

    /// `LPUSH` — prepend each value in turn; returns the new length.
    pub fn lpush(&mut self, key: &[u8], values: &[Vec<u8>]) -> Result<usize, StoreError> {
        let borrowed: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        self.lpush_borrowed(key, &borrowed)
    }

    /// `RPUSH` — append each value; returns the new length.
    pub fn rpush(&mut self, key: &[u8], values: &[Vec<u8>]) -> Result<usize, StoreError> {
        let borrowed: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        self.rpush_borrowed(key, &borrowed)
    }

    /// G4 (v1.25): borrowed-slice `LPUSH`. A.8: encoding-switch.
    pub fn lpush_borrowed(
        &mut self,
        key: &[u8],
        values: &[&[u8]],
    ) -> Result<usize, StoreError> {
        if values.is_empty() {
            return Ok(self.list_len(key));
        }
        let mut delta: i64 = 0;
        for v in values {
            delta += self.list_push_one(key, v, /* front= */ true)?;
        }
        self.account_delta(key, delta);
        Ok(self.list_len(key))
    }

    /// G4 (v1.25): borrowed-slice `RPUSH`. A.8: encoding-switch.
    pub fn rpush_borrowed(
        &mut self,
        key: &[u8],
        values: &[&[u8]],
    ) -> Result<usize, StoreError> {
        if values.is_empty() {
            return Ok(self.list_len(key));
        }
        let mut delta: i64 = 0;
        for v in values {
            delta += self.list_push_one(key, v, /* front= */ false)?;
        }
        self.account_delta(key, delta);
        Ok(self.list_len(key))
    }

    /// Push one element, applying the encoding-switch. Returns the
    /// per-element weight delta (zero for inline, list_item_weight for
    /// heap). `front=true` for LPUSH, `false` for RPUSH.
    fn list_push_one(&mut self, key: &[u8], v: &[u8], front: bool) -> Result<i64, StoreError> {
        if self.list_value_for_push(key)?.is_none() {
            return Ok(self.list_push_create(key, v));
        }
        let slot = self.list_value_for_push(key)?.expect("present and a list");
        match slot {
            Value::SmallListInline(s) => {
                let push = if front { s.try_push_front(v) } else { s.try_push_back(v) };
                match push {
                    PushResult::Pushed => Ok(0),
                    PushResult::NoRoom => {
                        let mut promoted = small_list::promote(s);
                        if front {
                            promoted.push_front(v.to_vec());
                        } else {
                            promoted.push_back(v.to_vec());
                        }
                        *slot = Value::List(Arc::new(promoted));
                        self.reweigh_entry(key);
                        // Reweighed from scratch — caller's delta should
                        // be 0 for THIS pair (already in the new weight).
                        Ok(0)
                    }
                }
            }
            Value::List(l) => {
                let l = Arc::make_mut(l);
                if front {
                    l.push_front(v.to_vec());
                } else {
                    l.push_back(v.to_vec());
                }
                Ok(list_item_weight(v.len()) as i64)
            }
            _ => Err(StoreError::WrongType),
        }
    }

    /// Create a fresh entry holding one element. Inline if it fits,
    /// else heap.
    fn list_push_create(&mut self, key: &[u8], v: &[u8]) -> i64 {
        if let Some(inline) = SmallListData::with_one(v) {
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::SmallListInline(inline), None),
            );
            0
        } else {
            let mut d = std::collections::VecDeque::with_capacity(1);
            d.push_back(v.to_vec());
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::List(Arc::new(d)), None),
            );
            0
        }
    }

    /// `LPOP` — pop up to `count` from the head (deleting emptied key).
    pub fn lpop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        // Inline → promote first if there is anything to pop; simpler
        // than maintaining a second pop path on the packed buffer.
        if matches!(self.map.get(key).map(|e| &e.value), Some(Value::SmallListInline(_))) {
            self.promote_list_inline_to_heap(key);
        }
        let (out, delta) = {
            let mut o = Vec::new();
            let mut d: i64 = 0;
            if let Some(l) = self.list_mut(key, false)? {
                for _ in 0..count {
                    match l.pop_front() {
                        Some(v) => {
                            d -= list_item_weight(v.len()) as i64;
                            o.push(v);
                        }
                        None => break,
                    }
                }
            }
            (o, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_list(key);
        Ok(out)
    }

    /// `RPOP` — pop up to `count` from the tail.
    pub fn rpop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        if matches!(self.map.get(key).map(|e| &e.value), Some(Value::SmallListInline(_))) {
            self.promote_list_inline_to_heap(key);
        }
        let (out, delta) = {
            let mut o = Vec::new();
            let mut d: i64 = 0;
            if let Some(l) = self.list_mut(key, false)? {
                for _ in 0..count {
                    match l.pop_back() {
                        Some(v) => {
                            d -= list_item_weight(v.len()) as i64;
                            o.push(v);
                        }
                        None => break,
                    }
                }
            }
            (o, d)
        };
        self.account_delta(key, delta);
        self.drop_if_empty_list(key);
        Ok(out)
    }

    /// Force-promote an inline list at `key` to its heap variant
    /// (no-op if already heap or absent). Used by mutating paths that
    /// only support the heap representation (pop/lrem/lset/ltrim).
    fn promote_list_inline_to_heap(&mut self, key: &[u8]) {
        let needs = matches!(
            self.map.get(key).map(|e| &e.value),
            Some(Value::SmallListInline(_))
        );
        if !needs {
            return;
        }
        let Some(e) = self.map.get_mut(key) else { return };
        if let Value::SmallListInline(s) = &e.value {
            let promoted = small_list::promote(s);
            e.value = Value::List(Arc::new(promoted));
        }
        self.reweigh_entry(key);
    }

    pub fn llen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        match self.live_entry(key) {
            None => Ok(0),
            Some(e) => match &e.value {
                Value::List(l) => Ok(l.len()),
                Value::SmallListInline(l) => Ok(l.len()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn lindex(&mut self, key: &[u8], idx: i64) -> Result<Option<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::List(l) => Ok(norm_index(idx, l.len()).and_then(|i| l.get(i).cloned())),
                Value::SmallListInline(l) => {
                    let n = l.len();
                    let Some(i) = norm_index(idx, n) else { return Ok(None) };
                    Ok(l.iter().nth(i).map(<[u8]>::to_vec))
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn lrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::List(l) => Ok(match range_bounds(start, stop, l.len()) {
                    None => Vec::new(),
                    Some((s, end)) => l.iter().skip(s).take(end - s + 1).cloned().collect(),
                }),
                Value::SmallListInline(l) => Ok(match range_bounds(start, stop, l.len()) {
                    None => Vec::new(),
                    Some((s, end)) => l
                        .iter()
                        .skip(s)
                        .take(end - s + 1)
                        .map(<[u8]>::to_vec)
                        .collect(),
                }),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `LSET` — errors with `NoSuchKey` / `OutOfRange` like Redis.
    pub fn lset(&mut self, key: &[u8], idx: i64, val: &[u8]) -> Result<(), StoreError> {
        self.promote_list_inline_to_heap(key);
        let delta = {
            let l = self.list_mut(key, false)?.ok_or(StoreError::NoSuchKey)?;
            let i = norm_index(idx, l.len()).ok_or(StoreError::OutOfRange)?;
            let old_len = l[i].len() as i64;
            l[i] = val.to_vec();
            val.len() as i64 - old_len
        };
        self.account_delta(key, delta);
        Ok(())
    }

    /// `LREM` — remove `count` occurrences (>0 head, <0 tail, 0 all).
    pub fn lrem(&mut self, key: &[u8], count: i64, val: &[u8]) -> Result<usize, StoreError> {
        self.promote_list_inline_to_heap(key);
        let (removed, delta) = {
            let mut r = 0usize;
            let mut d: i64 = 0;
            match self.list_mut(key, false)? {
                None => (0, 0),
                Some(l) => {
                    if count >= 0 {
                        let limit = if count == 0 {
                            usize::MAX
                        } else {
                            count as usize
                        };
                        let mut i = 0;
                        while i < l.len() {
                            if r < limit && l[i] == val {
                                d -= list_item_weight(l[i].len()) as i64;
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
                                d -= list_item_weight(l[i].len()) as i64;
                                l.remove(i);
                                r += 1;
                            }
                        }
                    }
                    (r, d)
                }
            }
        };
        self.account_delta(key, delta);
        self.drop_if_empty_list(key);
        Ok(removed)
    }

    /// `LTRIM` — keep only `[start, stop]` (deleting emptied key).
    pub fn ltrim(&mut self, key: &[u8], start: i64, stop: i64) -> Result<(), StoreError> {
        self.promote_list_inline_to_heap(key);
        let delta = {
            let mut d: i64 = 0;
            if let Some(l) = self.list_mut(key, false)? {
                match range_bounds(start, stop, l.len()) {
                    None => {
                        for v in l.iter() {
                            d -= list_item_weight(v.len()) as i64;
                        }
                        l.clear();
                    }
                    Some((s, e)) => {
                        for v in l.iter().skip(e + 1) {
                            d -= list_item_weight(v.len()) as i64;
                        }
                        l.drain(e + 1..);
                        for v in l.iter().take(s) {
                            d -= list_item_weight(v.len()) as i64;
                        }
                        l.drain(..s);
                    }
                }
            }
            d
        };
        self.account_delta(key, delta);
        self.drop_if_empty_list(key);
        Ok(())
    }
}
