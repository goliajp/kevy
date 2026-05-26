//! `Store` hash commands.

use crate::util::*;
use crate::value::*;
use crate::{Entry, Store, StoreError};
use std::time::Instant;

impl Store {
    // ---- hashes --------------------------------------------------------

    /// Borrow the key's hash mutably, optionally creating it. `Ok(None)` means
    /// the key is absent and `create` was false.
    fn hash_mut(&mut self, key: &[u8], create: bool) -> Result<Option<&mut HashData>, StoreError> {
        if self.live_entry_mut(key).is_none() {
            if !create {
                return Ok(None);
            }
            self.map.insert(
                key.to_vec(),
                Entry {
                    value: Value::Hash(Box::default()),
                    expire_at: None,
                },
            );
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::Hash(h) => Ok(Some(h)),
            _ => Err(StoreError::WrongType),
        }
    }

    /// Read the key's hash immutably (lazily expiring). `Ok(None)` if absent.
    fn hash_ref(&mut self, key: &[u8]) -> Result<Option<&HashData>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(Some(h)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `HSET` — returns the count of newly-added fields.
    pub fn hset(&mut self, key: &[u8], pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<usize, StoreError> {
        let h = self.hash_mut(key, true)?.expect("created");
        let mut added = 0;
        for (f, v) in pairs {
            if h.insert(f.clone(), v.clone()).is_none() {
                added += 1;
            }
        }
        Ok(added)
    }

    /// `HSETNX` — set only if the field is absent; returns whether it was set.
    pub fn hsetnx(&mut self, key: &[u8], field: &[u8], val: &[u8]) -> Result<bool, StoreError> {
        let h = self.hash_mut(key, true)?.expect("created");
        if h.contains_key(field) {
            // Don't leave an empty hash behind if we just created it.
            if h.is_empty() {
                self.map.remove(key);
            }
            return Ok(false);
        }
        h.insert(field.to_vec(), val.to_vec());
        Ok(true)
    }

    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> Result<Option<&[u8]>, StoreError> {
        Ok(self
            .hash_ref(key)?
            .and_then(|h| h.get(field))
            .map(|v| v.as_slice()))
    }

    pub fn hexists(&mut self, key: &[u8], field: &[u8]) -> Result<bool, StoreError> {
        Ok(self.hash_ref(key)?.is_some_and(|h| h.contains_key(field)))
    }

    pub fn hlen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.hash_ref(key)?.map_or(0, |h| h.len()))
    }

    pub fn hmget(
        &mut self,
        key: &[u8],
        fields: &[Vec<u8>],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        let h = self.hash_ref(key)?;
        Ok(fields
            .iter()
            .map(|f| h.and_then(|h| h.get(f)).cloned())
            .collect())
    }

    /// `HGETALL` — flat `[field, value, field, value, ...]` (clones; perf-polish later).
    pub fn hgetall(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.hash_ref(key)? {
            None => Ok(Vec::new()),
            Some(h) => {
                let mut out = Vec::with_capacity(h.len() * 2);
                for (f, v) in h {
                    out.push(f.clone());
                    out.push(v.clone());
                }
                Ok(out)
            }
        }
    }

    pub fn hkeys(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(self
            .hash_ref(key)?
            .map_or(Vec::new(), |h| h.keys().cloned().collect()))
    }

    pub fn hvals(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(self
            .hash_ref(key)?
            .map_or(Vec::new(), |h| h.values().cloned().collect()))
    }

    /// `HDEL` — returns count removed; deletes the key if the hash becomes empty.
    pub fn hdel(&mut self, key: &[u8], fields: &[Vec<u8>]) -> Result<usize, StoreError> {
        let now = Instant::now();
        if !self.reap(key, now) {
            return Ok(0);
        }
        let removed = match &mut self.map.get_mut(key).expect("live").value {
            Value::Hash(h) => fields.iter().filter(|f| h.remove(*f).is_some()).count(),
            _ => return Err(StoreError::WrongType),
        };
        if let Some(Value::Hash(h)) = self.map.get(key).map(|e| &e.value)
            && h.is_empty()
        {
            self.map.remove(key);
        }
        Ok(removed)
    }

    /// `HINCRBY` — preserves TTL; errors if the field isn't an integer.
    pub fn hincrby(&mut self, key: &[u8], field: &[u8], delta: i64) -> Result<i64, StoreError> {
        let h = self.hash_mut(key, true)?.expect("created");
        let cur = match h.get(field) {
            Some(v) => parse_i64(v).ok_or(StoreError::NotInteger)?,
            None => 0,
        };
        let next = cur.checked_add(delta).ok_or(StoreError::Overflow)?;
        h.insert(field.to_vec(), next.to_string().into_bytes());
        Ok(next)
    }
}
