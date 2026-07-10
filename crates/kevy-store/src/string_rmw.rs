//! Read-modify-write string ops split out of `string.rs` (500-LOC
//! rule): `APPEND` / `GETSET` / `GETDEL` / `INCRBYFLOAT`. All four are
//! encoding-aware across `Value::Str` / `Value::Int` / `Value::ArcBulk`
//! (fixed 2026-07-03 — they carried pre-L2 `Str`-only arms and replied
//! WRONGTYPE where Redis succeeds; guard: `tests_string_encoding.rs`).

use crate::string_set::pick_value_for_set_owned;
use crate::util::{fmt_num, format_i64_into, itoa_i64_stack, parse_f64};
use crate::value::{SmallBytes, Value};
use crate::{Entry, Store, StoreError};

impl Store {
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
                // L2: Int-encoded value — materialise the digits, append,
                // then re-pick the encoding ("1" + "2" = canonical "12"
                // goes straight back to Int so follow-up INCR stays fast).
                Value::Int(n) => {
                    let mut buf = itoa_i64_stack();
                    let mut owned = format_i64_into(*n, &mut buf).to_vec();
                    owned.extend_from_slice(data);
                    let new_len = owned.len();
                    e.value = pick_value_for_set_owned(owned);
                    AppendOutcome::Reweigh(new_len)
                }
                // L1: APPEND on Arc-backed bulk → materialise to a fresh
                // Vec (no other reader has refs to the old Arc post-replace),
                // append, then pick the new encoding via SET routing rules.
                Value::ArcBulk(a) => {
                    let mut owned: Vec<u8> = a.as_ref().to_vec();
                    owned.extend_from_slice(data);
                    let new_len = owned.len();
                    e.value = pick_value_for_set_owned(owned);
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
    /// `GETSET` — set to `val`, return the previous string (WRONGTYPE if the old
    /// value isn't a string). Clears any TTL, like SET.
    pub fn getset(&mut self, key: &[u8], val: Vec<u8>) -> Result<Option<Vec<u8>>, StoreError> {
        let old = match self.live_entry(key) {
            Some(e) => match &e.value {
                Value::Str(v) => Some(v.to_vec()),
                Value::Int(n) => {
                    let mut buf = itoa_i64_stack();
                    Some(format_i64_into(*n, &mut buf).to_vec())
                }
                Value::ArcBulk(a) => Some(a.as_ref().to_vec()),
                _ => return Err(StoreError::WrongType),
            },
            None => None,
        };
        // Route through the SET encoding rules so a canonical-int new
        // value takes the Int fast path, same as a plain SET would.
        self.insert_entry(
            SmallBytes::from_slice(key),
            Entry::new(pick_value_for_set_owned(val), None),
        );
        Ok(old)
    }

    /// `GETDEL` — get then delete (WRONGTYPE if non-string).
    pub fn getdel(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => return Ok(None),
            Some(e) => match &e.value {
                Value::Str(_) | Value::Int(_) | Value::ArcBulk(_) => {}
                _ => return Err(StoreError::WrongType),
            },
        }
        match self.remove_entry(key) {
            Some(Entry {
                value: Value::Str(v),
                ..
            }) => Ok(Some(v.into_vec())),
            Some(Entry {
                value: Value::Int(n),
                ..
            }) => {
                let mut buf = itoa_i64_stack();
                Ok(Some(format_i64_into(n, &mut buf).to_vec()))
            }
            Some(Entry {
                value: Value::ArcBulk(a),
                ..
            }) => Ok(Some(a.as_ref().to_vec())),
            _ => Ok(None),
        }
    }

    /// `INCRBYFLOAT` — returns the new value formatted as Redis would. Preserves TTL.
    pub fn incr_by_float(&mut self, key: &[u8], delta: f64) -> Result<Vec<u8>, StoreError> {
        let outcome = if let Some(e) = self.live_entry_mut(key) { match &mut e.value {
            Value::Str(v) => {
                let cur = parse_f64(v.as_slice()).ok_or(StoreError::NotFloat)?;
                let bytes = float_incr_bytes(cur, delta)?;
                *v = SmallBytes::from_slice(&bytes);
                FloatOutcome::Reweigh(bytes)
            }
            Value::Int(n) => {
                let bytes = float_incr_bytes(*n as f64, delta)?;
                e.value = Value::Str(SmallBytes::from_slice(&bytes));
                FloatOutcome::Reweigh(bytes)
            }
            Value::ArcBulk(a) => {
                let cur = parse_f64(a.as_ref()).ok_or(StoreError::NotFloat)?;
                let bytes = float_incr_bytes(cur, delta)?;
                e.value = Value::Str(SmallBytes::from_slice(&bytes));
                FloatOutcome::Reweigh(bytes)
            }
            _ => return Err(StoreError::WrongType),
        } } else {
            // Absent/expired ⇒ start from 0.0.
            if !delta.is_finite() {
                return Err(StoreError::NotFloat);
            }
            FloatOutcome::Insert(fmt_num(delta))
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

enum FloatOutcome {
    Reweigh(Vec<u8>),
    Insert(Vec<u8>),
}

/// Shared tail of the three INCRBYFLOAT arms: add `delta`, reject a
/// non-finite result (Redis `NaN`/`inf` guard), format Redis-style.
#[inline]
fn float_incr_bytes(cur: f64, delta: f64) -> Result<Vec<u8>, StoreError> {
    let next = cur + delta;
    if !next.is_finite() {
        return Err(StoreError::NotFloat);
    }
    Ok(fmt_num(next))
}
