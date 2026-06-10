//! `Store` string commands.

use crate::util::*;
use crate::value::*;
use crate::{Entry, Store, StoreError};
use std::time::{Duration, Instant};

impl Store {
    // ---- strings -------------------------------------------------------

    /// `SET` — overwrites any existing value/type. NX/XX guards; clears TTL.
    pub fn set(
        &mut self,
        key: &[u8],
        value: Vec<u8>,
        expire: Option<Duration>,
        nx: bool,
        xx: bool,
    ) -> bool {
        // Clock read only when a TTL is requested.
        let expire_at = expire.map(|d| Instant::now() + d);
        let new_value = Value::Str(SmallBytes::from_vec(value));
        let key_heap = crate::key_heap_bytes_for(key);
        let outcome = match self.live_entry_mut(key) {
            // Key exists and is live: NX must abort; otherwise overwrite the
            // value + TTL in place — no `key.to_vec()` (the key is already in
            // the table, std `insert` would clone it only to drop it). The
            // weight delta is computed HERE on the `&mut Entry` we already
            // hold — `reweigh_entry(key)` would re-hash + re-probe the map
            // for the entry we just mutated (the overwrite-SET hot path).
            Some(e) => {
                if nx {
                    return false;
                }
                e.value = new_value;
                e.expire_at_ns = expire_at.and_then(crate::pack_deadline);
                let new_w = key_heap + e.value.weight();
                let delta = new_w as i64 - e.weight() as i64;
                e.set_weight(new_w);
                Ok(delta)
            }
            // Absent (or expired ⇒ already dropped by live_entry_mut): XX aborts.
            None => {
                if xx {
                    return false;
                }
                Err(Entry::new(new_value, expire_at))
            }
        };
        match outcome {
            Ok(delta) => self.apply_weight_delta(delta),
            Err(entry) => {
                self.insert_entry(SmallBytes::from_slice(key), entry);
            }
        }
        true
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<&[u8]>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Str(v) => Ok(Some(v.as_slice())),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Read-only `GET`: `&self`, so concurrent readers can run under a shared
    /// lock (embedded mode's `RwLock` read path). Expiry is checked against the
    /// coarse cached clock but an expired key is *not* removed here (no `&mut`)
    /// — the reaper / next write reclaims it; a reader just sees `None`. LRU is
    /// not touched, so this path is only used when eviction is off
    /// (`maxmemory == 0`); with eviction, the caller takes the mutating
    /// [`Self::get`] under an exclusive lock so access still stamps the LRU.
    pub fn get_shared(&self, key: &[u8]) -> Result<Option<&[u8]>, StoreError> {
        match self.map.get(key) {
            None => Ok(None),
            Some(e) if e.is_expired(self.cached_clock, self.cached_ns) => Ok(None),
            Some(e) => match &e.value {
                Value::Str(v) => Ok(Some(v.as_slice())),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn strlen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.get(key)?.map_or(0, |v| v.len()))
    }

    pub fn append(&mut self, key: &[u8], data: &[u8]) -> Result<usize, StoreError> {
        let outcome = match self.live_entry_mut(key) {
            Some(e) => match &mut e.value {
                Value::Str(v) => {
                    // SmallBytes is immutable; pop out, grow via Vec, re-wrap.
                    let mut owned = std::mem::take(v).into_vec();
                    owned.extend_from_slice(data);
                    let new_len = owned.len();
                    *v = SmallBytes::from_vec(owned);
                    AppendOutcome::Reweigh(new_len)
                }
                _ => return Err(StoreError::WrongType),
            },
            None => AppendOutcome::Insert,
        };
        match outcome {
            AppendOutcome::Reweigh(new_len) => {
                self.reweigh_entry(key);
                Ok(new_len)
            }
            AppendOutcome::Insert => {
                self.insert_entry(
                    SmallBytes::from_slice(key),
                    Entry::new(Value::Str(SmallBytes::from_slice(data)), None),
                );
                Ok(data.len())
            }
        }
    }

    /// `INCRBY` family; preserves any TTL.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> Result<i64, StoreError> {
        let outcome = match self.live_entry_mut(key) {
            Some(e) => match &mut e.value {
                Value::Str(v) => {
                    let next = parse_i64(v.as_slice())
                        .ok_or(StoreError::NotInteger)?
                        .checked_add(delta)
                        .ok_or(StoreError::Overflow)?;
                    *v = SmallBytes::from_vec(next.to_string().into_bytes());
                    IncrOutcome::Reweigh(next)
                }
                _ => return Err(StoreError::WrongType),
            },
            // Absent/expired ⇒ start from 0; 0 + delta can't overflow i64.
            None => IncrOutcome::Insert(delta),
        };
        match outcome {
            IncrOutcome::Reweigh(next) => {
                self.reweigh_entry(key);
                Ok(next)
            }
            IncrOutcome::Insert(next) => {
                self.insert_entry(
                    SmallBytes::from_slice(key),
                    Entry::new(
                        Value::Str(SmallBytes::from_vec(next.to_string().into_bytes())),
                        None,
                    ),
                );
                Ok(next)
            }
        }
    }

    /// `GETSET` — set to `val`, return the previous string (WRONGTYPE if the old
    /// value isn't a string). Clears any TTL, like SET.
    pub fn getset(&mut self, key: &[u8], val: Vec<u8>) -> Result<Option<Vec<u8>>, StoreError> {
        let old = match self.live_entry(key) {
            Some(e) => match &e.value {
                Value::Str(v) => Some(v.to_vec()),
                _ => return Err(StoreError::WrongType),
            },
            None => None,
        };
        self.insert_entry(
            SmallBytes::from_slice(key),
            Entry::new(Value::Str(SmallBytes::from_vec(val)), None),
        );
        Ok(old)
    }

    /// `GETDEL` — get then delete (WRONGTYPE if non-string).
    pub fn getdel(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        let is_str = match self.live_entry(key) {
            None => return Ok(None),
            Some(e) => matches!(e.value, Value::Str(_)),
        };
        if !is_str {
            return Err(StoreError::WrongType);
        }
        match self.remove_entry(key) {
            Some(Entry {
                value: Value::Str(v),
                ..
            }) => Ok(Some(v.into_vec())),
            _ => Ok(None),
        }
    }

    /// `INCRBYFLOAT` — returns the new value formatted as Redis would. Preserves TTL.
    pub fn incr_by_float(&mut self, key: &[u8], delta: f64) -> Result<Vec<u8>, StoreError> {
        let outcome = match self.live_entry_mut(key) {
            Some(e) => match &mut e.value {
                Value::Str(v) => {
                    let next = parse_f64(v.as_slice()).ok_or(StoreError::NotFloat)? + delta;
                    if !next.is_finite() {
                        return Err(StoreError::NotFloat);
                    }
                    let bytes = fmt_num(next);
                    *v = SmallBytes::from_slice(&bytes);
                    FloatOutcome::Reweigh(bytes)
                }
                _ => return Err(StoreError::WrongType),
            },
            None => {
                // Absent/expired ⇒ start from 0.0.
                if !delta.is_finite() {
                    return Err(StoreError::NotFloat);
                }
                FloatOutcome::Insert(fmt_num(delta))
            }
        };
        match outcome {
            FloatOutcome::Reweigh(bytes) => {
                self.reweigh_entry(key);
                Ok(bytes)
            }
            FloatOutcome::Insert(bytes) => {
                self.insert_entry(
                    SmallBytes::from_slice(key),
                    Entry::new(Value::Str(SmallBytes::from_slice(&bytes)), None),
                );
                Ok(bytes)
            }
        }
    }
}

enum AppendOutcome {
    Reweigh(usize),
    Insert,
}

enum IncrOutcome {
    Reweigh(i64),
    Insert(i64),
}

enum FloatOutcome {
    Reweigh(Vec<u8>),
    Insert(Vec<u8>),
}
