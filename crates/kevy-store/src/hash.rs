//! `Store` hash commands.

use crate::util::*;
use crate::value::*;
use crate::{Entry, Store, StoreError};
use std::sync::Arc;
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
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::Hash(Arc::default()), None),
            );
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::Hash(h) => Ok(Some(Arc::make_mut(h))),
            _ => Err(StoreError::WrongType),
        }
    }

    /// Read the key's hash immutably (lazily expiring). `Ok(None)` if absent.
    fn hash_ref(&mut self, key: &[u8]) -> Result<Option<&HashData>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(Some(h.as_ref())),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `HSET` — returns the count of newly-added fields.
    pub fn hset(&mut self, key: &[u8], pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<usize, StoreError> {
        let (added, delta) = {
            let h = self.hash_mut(key, true)?.expect("created");
            let mut a = 0usize;
            let mut d: i64 = 0;
            for (f, v) in pairs {
                let smb = SmallBytes::from_slice(f);
                let new_w = hash_field_weight(&smb, v.len()) as i64;
                match h.insert(smb, v.clone()) {
                    None => {
                        a += 1;
                        d += new_w;
                    }
                    Some(old) => {
                        d += v.len() as i64 - old.len() as i64;
                    }
                }
            }
            (a, d)
        };
        self.account_delta(key, delta);
        Ok(added)
    }

    /// `HSETNX` — set only if the field is absent; returns whether it was set.
    pub fn hsetnx(&mut self, key: &[u8], field: &[u8], val: &[u8]) -> Result<bool, StoreError> {
        let outcome = {
            let h = self.hash_mut(key, true)?.expect("created");
            if h.contains_key(field) {
                if h.is_empty() {
                    HsetnxOutcome::DropEmpty
                } else {
                    HsetnxOutcome::AlreadyExists
                }
            } else {
                let smb = SmallBytes::from_slice(field);
                let w = hash_field_weight(&smb, val.len()) as i64;
                h.insert(smb, val.to_vec());
                HsetnxOutcome::Inserted(w)
            }
        };
        match outcome {
            HsetnxOutcome::DropEmpty => {
                self.remove_entry(key);
                Ok(false)
            }
            HsetnxOutcome::AlreadyExists => Ok(false),
            HsetnxOutcome::Inserted(w) => {
                self.account_delta(key, w);
                Ok(true)
            }
        }
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
            .map(|f| h.and_then(|h| h.get(f.as_slice())).cloned())
            .collect())
    }

    /// `HGETALL` — flat `[field, value, field, value, ...]` (clones; perf-polish later).
    pub fn hgetall(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.hash_ref(key)? {
            None => Ok(Vec::new()),
            Some(h) => {
                let mut out = Vec::with_capacity(h.len() * 2);
                for (f, v) in h {
                    out.push(f.to_vec());
                    out.push(v.clone());
                }
                Ok(out)
            }
        }
    }

    pub fn hkeys(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        Ok(self
            .hash_ref(key)?
            .map_or(Vec::new(), |h| h.keys().map(|k| k.to_vec()).collect()))
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
        let (removed, delta, drop_key) = {
            let h_entry = self.map.get_mut(key).expect("live");
            match &mut h_entry.value {
                Value::Hash(h) => {
                    let h = Arc::make_mut(h);
                    let mut r = 0usize;
                    let mut d: i64 = 0;
                    for f in fields {
                        if let Some(old_v) = h.remove(f.as_slice()) {
                            r += 1;
                            // The field key matters as a SmallBytes only for
                            // heap_bytes/slot overhead; reconstruct the same
                            // weight figure that hset paid in.
                            let smb = SmallBytes::from_slice(f);
                            d -= hash_field_weight(&smb, old_v.len()) as i64;
                        }
                    }
                    let drop_now = h.is_empty();
                    (r, d, drop_now)
                }
                _ => return Err(StoreError::WrongType),
            }
        };
        if drop_key {
            self.remove_entry(key);
        } else {
            self.account_delta(key, delta);
        }
        Ok(removed)
    }

    /// `HINCRBY` — preserves TTL; errors if the field isn't an integer.
    pub fn hincrby(&mut self, key: &[u8], field: &[u8], delta: i64) -> Result<i64, StoreError> {
        let (next, weight_delta) = {
            let h = self.hash_mut(key, true)?.expect("created");
            let cur = match h.get(field) {
                Some(v) => parse_i64(v).ok_or(StoreError::NotInteger)?,
                None => 0,
            };
            let next = cur.checked_add(delta).ok_or(StoreError::Overflow)?;
            let new_bytes = next.to_string().into_bytes();
            let smb = SmallBytes::from_slice(field);
            let new_field_w = hash_field_weight(&smb, new_bytes.len()) as i64;
            let new_value_len = new_bytes.len();
            let wd = match h.insert(smb, new_bytes) {
                None => new_field_w,
                Some(old) => new_value_len as i64 - old.len() as i64,
            };
            (next, wd)
        };
        self.account_delta(key, weight_delta);
        Ok(next)
    }
}

enum HsetnxOutcome {
    DropEmpty,
    AlreadyExists,
    Inserted(i64),
}
