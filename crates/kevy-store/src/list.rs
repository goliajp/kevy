//! `Store` list commands.

use crate::util::{norm_index, range_bounds};
use crate::value::{ListData, SmallBytes, Value, list_item_weight};
use crate::{Entry, Store, StoreError};
use std::sync::Arc;

impl Store {
    // ---- lists ---------------------------------------------------------

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
        match &mut self.map.get_mut(key).expect("present").value {
            Value::List(l) => Ok(Some(Arc::make_mut(l))),
            _ => Err(StoreError::WrongType),
        }
    }

    fn list_ref(&mut self, key: &[u8]) -> Result<Option<&ListData>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::List(l) => Ok(Some(l.as_ref())),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Remove `key` if it now holds an empty list.
    fn drop_if_empty_list(&mut self, key: &[u8]) {
        let empty = matches!(self.map.get(key).map(|e| &e.value), Some(Value::List(l)) if l.is_empty());
        if empty {
            self.remove_entry(key);
        }
    }

    /// `LPUSH` — prepend each value in turn; returns the new length.
    pub fn lpush(&mut self, key: &[u8], values: &[Vec<u8>]) -> Result<usize, StoreError> {
        let (new_len, delta) = {
            let l = self.list_mut(key, true)?.expect("created");
            let mut d: i64 = 0;
            for v in values {
                d += list_item_weight(v.len()) as i64;
                l.push_front(v.clone());
            }
            (l.len(), d)
        };
        self.account_delta(key, delta);
        Ok(new_len)
    }

    /// `RPUSH` — append each value; returns the new length.
    pub fn rpush(&mut self, key: &[u8], values: &[Vec<u8>]) -> Result<usize, StoreError> {
        let (new_len, delta) = {
            let l = self.list_mut(key, true)?.expect("created");
            let mut d: i64 = 0;
            for v in values {
                d += list_item_weight(v.len()) as i64;
                l.push_back(v.clone());
            }
            (l.len(), d)
        };
        self.account_delta(key, delta);
        Ok(new_len)
    }

    /// `LPOP` — pop up to `count` from the head (deleting an emptied key).
    pub fn lpop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
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

    pub fn llen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.list_ref(key)?.map_or(0, std::collections::VecDeque::len))
    }

    pub fn lindex(&mut self, key: &[u8], idx: i64) -> Result<Option<Vec<u8>>, StoreError> {
        match self.list_ref(key)? {
            None => Ok(None),
            Some(l) => Ok(norm_index(idx, l.len()).and_then(|i| l.get(i).cloned())),
        }
    }

    pub fn lrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.list_ref(key)? {
            None => Ok(Vec::new()),
            Some(l) => Ok(match range_bounds(start, stop, l.len()) {
                None => Vec::new(),
                Some((s, e)) => l.iter().skip(s).take(e - s + 1).cloned().collect(),
            }),
        }
    }

    /// `LSET` — errors with `NoSuchKey` / `OutOfRange` like Redis.
    pub fn lset(&mut self, key: &[u8], idx: i64, val: &[u8]) -> Result<(), StoreError> {
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

    /// `LREM` — remove `count` occurrences of `val` (>0 head, <0 tail, 0 all).
    pub fn lrem(&mut self, key: &[u8], count: i64, val: &[u8]) -> Result<usize, StoreError> {
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

    /// `LTRIM` — keep only `[start, stop]` (deleting an emptied key).
    pub fn ltrim(&mut self, key: &[u8], start: i64, stop: i64) -> Result<(), StoreError> {
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
