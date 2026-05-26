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
        match self.live_entry_mut(key) {
            // Key exists and is live: NX must abort; otherwise overwrite the
            // value + TTL in place — no `key.to_vec()` (the key is already in
            // the table, std `insert` would clone it only to drop it).
            Some(e) => {
                if nx {
                    return false;
                }
                e.value = Value::Str(SmallBytes::from_vec(value));
                e.expire_at = expire_at;
                true
            }
            // Absent (or expired ⇒ already dropped by live_entry_mut): XX aborts.
            None => {
                if xx {
                    return false;
                }
                self.map.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Str(SmallBytes::from_vec(value)),
                        expire_at,
                    },
                );
                true
            }
        }
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

    pub fn strlen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        Ok(self.get(key)?.map_or(0, |v| v.len()))
    }

    pub fn append(&mut self, key: &[u8], data: &[u8]) -> Result<usize, StoreError> {
        match self.live_entry_mut(key) {
            Some(e) => match &mut e.value {
                Value::Str(v) => {
                    // SmallBytes is immutable; pop out, grow via Vec, re-wrap.
                    let mut owned = std::mem::take(v).into_vec();
                    owned.extend_from_slice(data);
                    let new_len = owned.len();
                    *v = SmallBytes::from_vec(owned);
                    Ok(new_len)
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                self.map.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Str(SmallBytes::from_slice(data)),
                        expire_at: None,
                    },
                );
                Ok(data.len())
            }
        }
    }

    /// `INCRBY` family; preserves any TTL.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> Result<i64, StoreError> {
        match self.live_entry_mut(key) {
            Some(e) => match &mut e.value {
                Value::Str(v) => {
                    let next = parse_i64(v.as_slice())
                        .ok_or(StoreError::NotInteger)?
                        .checked_add(delta)
                        .ok_or(StoreError::Overflow)?;
                    *v = SmallBytes::from_vec(next.to_string().into_bytes());
                    Ok(next)
                }
                _ => Err(StoreError::WrongType),
            },
            // Absent/expired ⇒ start from 0; 0 + delta can't overflow i64.
            None => {
                self.map.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Str(SmallBytes::from_vec(delta.to_string().into_bytes())),
                        expire_at: None,
                    },
                );
                Ok(delta)
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
        self.map.insert(
            key.to_vec(),
            Entry {
                value: Value::Str(SmallBytes::from_vec(val)),
                expire_at: None,
            },
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
        match self.map.remove(key) {
            Some(Entry {
                value: Value::Str(v),
                ..
            }) => Ok(Some(v.into_vec())),
            _ => Ok(None),
        }
    }

    /// `INCRBYFLOAT` — returns the new value formatted as Redis would. Preserves TTL.
    pub fn incr_by_float(&mut self, key: &[u8], delta: f64) -> Result<Vec<u8>, StoreError> {
        match self.live_entry_mut(key) {
            Some(e) => match &mut e.value {
                Value::Str(v) => {
                    let next = parse_f64(v.as_slice()).ok_or(StoreError::NotFloat)? + delta;
                    if !next.is_finite() {
                        return Err(StoreError::NotFloat);
                    }
                    let bytes = fmt_num(next);
                    *v = SmallBytes::from_slice(&bytes);
                    Ok(bytes)
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                // Absent/expired ⇒ start from 0.0.
                if !delta.is_finite() {
                    return Err(StoreError::NotFloat);
                }
                let bytes = fmt_num(delta);
                self.map.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Str(SmallBytes::from_slice(&bytes)),
                        expire_at: None,
                    },
                );
                Ok(bytes)
            }
        }
    }
}
