//! `Store` list commands.

use crate::util::*;
use crate::value::*;
use crate::{Entry, Store, StoreError};

impl Store {
    // ---- lists ---------------------------------------------------------

    fn list_mut(&mut self, key: &[u8], create: bool) -> Result<Option<&mut ListData>, StoreError> {
        if self.live_entry_mut(key).is_none() {
            if !create {
                return Ok(None);
            }
            self.map.insert(
                key.to_vec(),
                Entry {
                    value: Value::List(Box::default()),
                    expire_at: None,
                },
            );
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::List(l) => Ok(Some(l)),
            _ => Err(StoreError::WrongType),
        }
    }

    fn list_ref(&mut self, key: &[u8]) -> Result<Option<&ListData>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::List(l) => Ok(Some(l)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Remove `key` if it now holds an empty list.
    fn drop_if_empty_list(&mut self, key: &[u8]) {
        if let Some(Value::List(l)) = self.map.get(key).map(|e| &e.value)
            && l.is_empty()
        {
            self.map.remove(key);
        }
    }

    /// `LPUSH` — prepend each value in turn; returns the new length.
    pub fn lpush(&mut self, key: &[u8], values: &[Vec<u8>]) -> Result<usize, StoreError> {
        let l = self.list_mut(key, true)?.expect("created");
        for v in values {
            l.push_front(v.clone());
        }
        Ok(l.len())
    }

    /// `RPUSH` — append each value; returns the new length.
    pub fn rpush(&mut self, key: &[u8], values: &[Vec<u8>]) -> Result<usize, StoreError> {
        let l = self.list_mut(key, true)?.expect("created");
        for v in values {
            l.push_back(v.clone());
        }
        Ok(l.len())
    }

    /// `LPOP` — pop up to `count` from the head (deleting an emptied key).
    pub fn lpop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut out = Vec::new();
        if let Some(l) = self.list_mut(key, false)? {
            for _ in 0..count {
                match l.pop_front() {
                    Some(v) => out.push(v),
                    None => break,
                }
            }
        }
        self.drop_if_empty_list(key);
        Ok(out)
    }

    /// `RPOP` — pop up to `count` from the tail.
    pub fn rpop(&mut self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut out = Vec::new();
        if let Some(l) = self.list_mut(key, false)? {
            for _ in 0..count {
                match l.pop_back() {
                    Some(v) => out.push(v),
                    None => break,
                }
            }
        }
        self.drop_if_empty_list(key);
        Ok(out)
    }

    pub fn llen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.list_ref(key)?.map_or(0, |l| l.len()))
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
        let l = self.list_mut(key, false)?.ok_or(StoreError::NoSuchKey)?;
        let i = norm_index(idx, l.len()).ok_or(StoreError::OutOfRange)?;
        l[i] = val.to_vec();
        Ok(())
    }

    /// `LREM` — remove `count` occurrences of `val` (>0 head, <0 tail, 0 all).
    pub fn lrem(&mut self, key: &[u8], count: i64, val: &[u8]) -> Result<usize, StoreError> {
        let removed = match self.list_mut(key, false)? {
            None => 0,
            Some(l) => {
                let mut removed = 0;
                if count >= 0 {
                    let limit = if count == 0 {
                        usize::MAX
                    } else {
                        count as usize
                    };
                    let mut i = 0;
                    while i < l.len() {
                        if removed < limit && l[i] == val {
                            l.remove(i);
                            removed += 1;
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    let limit = (-count) as usize;
                    let mut i = l.len();
                    while i > 0 {
                        i -= 1;
                        if removed < limit && l[i] == val {
                            l.remove(i);
                            removed += 1;
                        }
                    }
                }
                removed
            }
        };
        self.drop_if_empty_list(key);
        Ok(removed)
    }

    /// `LTRIM` — keep only `[start, stop]` (deleting an emptied key).
    pub fn ltrim(&mut self, key: &[u8], start: i64, stop: i64) -> Result<(), StoreError> {
        if let Some(l) = self.list_mut(key, false)? {
            match range_bounds(start, stop, l.len()) {
                None => l.clear(),
                Some((s, e)) => {
                    l.drain(e + 1..);
                    l.drain(..s);
                }
            }
        }
        self.drop_if_empty_list(key);
        Ok(())
    }
}
