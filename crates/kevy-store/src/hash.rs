//! `Store` hash commands.

use crate::small_hash::{self, AddResult as HAddResult, SmallHashData};
use crate::util::{parse_f64, parse_i64};
use crate::value::{HashData, SmallBytes, Value, hash_field_weight};
use crate::{Entry, Store, StoreError, now_ns};
use std::sync::Arc;

impl Store {
    // ---- hashes --------------------------------------------------------

    /// Borrow the key's hash mutably, optionally creating it. `Ok(None)` means
    /// the key is absent and `create` was false.
    ///
    /// A.8: only used by the heap-backed legacy read/mutate sites
    /// (`hincrby`, the `hash_ref` reader). The bulk writers (`hset` /
    /// `hdel`) take the encoding-switch path via `hset_one` /
    /// `hash_field_get_mut_for_hset`. When `create=true` on a missing
    /// key, materialises a heap `Value::Hash(Arc::default())` (no inline
    /// path), matching pre-A.8 behaviour for read-modify-write entry
    /// points that don't carry per-pair size info.
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
        // A.8: detect inline-encoding first (independent borrow), then
        // upgrade out-of-scope of the &mut, then re-borrow as Hash. The
        // borrow checker rejects the obvious in-place match because
        // both arms would return a borrow tied to the same `&mut self`.
        let is_inline = matches!(
            self.map.get(key).map(|e| &e.value),
            Some(Value::SmallHashInline(_))
        );
        if is_inline {
            let promoted = {
                let e = self.map.get(key).expect("present");
                if let Value::SmallHashInline(s) = &e.value {
                    small_hash::promote(s)
                } else {
                    unreachable!()
                }
            };
            self.map.get_mut(key).expect("present").value = Value::Hash(Arc::new(promoted));
            self.reweigh_entry(key);
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::Hash(h) => Ok(Some(Arc::make_mut(h))),
            _ => Err(StoreError::WrongType),
        }
    }

    /// A.8: read the key's hash slot for HSET. `WrongType` if the entry
    /// is not a hash. Returns `None` when the key is absent — caller
    /// (`hset_one`) creates the entry then.
    fn hash_value_for_set(&mut self, key: &[u8]) -> Result<Option<&mut Value>, StoreError> {
        match self.live_entry_mut(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(_) | Value::SmallHashInline(_) => Ok(Some(&mut e.value)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Read the key's hash immutably (lazily expiring) — returns the
    /// pairs as a vector of `(&[u8], &[u8])`. None if absent.
    /// Internal helper for read-only paths; collects into a new Vec to
    /// avoid the two-encoding match dance at every callsite.
    fn hash_pairs(&mut self, key: &[u8]) -> Result<Option<Vec<(Vec<u8>, Vec<u8>)>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(Some(
                    h.iter().map(|(f, v)| (f.to_vec(), v.clone())).collect(),
                )),
                Value::SmallHashInline(h) => Ok(Some(
                    h.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect(),
                )),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// G4 (v1.25): borrowed-pair `HSET` — kills the per-field+value
    /// `Vec<u8>` allocs the dispatch layer used to do before calling
    /// [`Self::hset`]. A.8: routes through the encoding-switch path.
    pub fn hset_borrowed(
        &mut self,
        key: &[u8],
        pairs: &[(&[u8], &[u8])],
    ) -> Result<usize, StoreError> {
        if pairs.is_empty() {
            return Ok(0);
        }
        let mut added = 0usize;
        let mut delta: i64 = 0;
        for (f, v) in pairs {
            match self.hset_one(key, f, v)? {
                HsetOutcome::AddedInline => {
                    added += 1;
                    // Inline carries zero heap delta — already accounted
                    // at insert_entry / per-call via value.weight()==0.
                }
                HsetOutcome::UpdatedInline => {}
                HsetOutcome::AddedHeap(w) => {
                    added += 1;
                    delta += w;
                }
                HsetOutcome::UpdatedHeap(d) => {
                    delta += d;
                }
            }
        }
        self.account_delta(key, delta);
        Ok(added)
    }

    /// `HSET` — returns the count of newly-added fields.
    pub fn hset(&mut self, key: &[u8], pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<usize, StoreError> {
        let borrowed: Vec<(&[u8], &[u8])> =
            pairs.iter().map(|(f, v)| (f.as_slice(), v.as_slice())).collect();
        self.hset_borrowed(key, &borrowed)
    }

    /// `HSETNX` — set only if the field is absent; returns whether it was set.
    pub fn hsetnx(&mut self, key: &[u8], field: &[u8], val: &[u8]) -> Result<bool, StoreError> {
        // Existing-field fast check via the encoding-aware reader.
        let exists = match self.live_entry(key) {
            None => false,
            Some(e) => match &e.value {
                Value::Hash(h) => h.contains_key(field),
                Value::SmallHashInline(h) => h.contains_key(field),
                _ => return Err(StoreError::WrongType),
            },
        };
        if exists {
            return Ok(false);
        }
        match self.hset_one(key, field, val)? {
            HsetOutcome::AddedInline | HsetOutcome::UpdatedInline => Ok(true),
            HsetOutcome::AddedHeap(w) => {
                self.account_delta(key, w);
                Ok(true)
            }
            HsetOutcome::UpdatedHeap(_) => Ok(true),
        }
    }

    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> Result<Option<&[u8]>, StoreError> {
        match self.live_entry(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.get(field).map(Vec::as_slice)),
                Value::SmallHashInline(h) => Ok(h.get(field)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn hexists(&mut self, key: &[u8], field: &[u8]) -> Result<bool, StoreError> {
        match self.live_entry(key) {
            None => Ok(false),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.contains_key(field)),
                Value::SmallHashInline(h) => Ok(h.contains_key(field)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn hlen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        match self.live_entry(key) {
            None => Ok(0),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.len()),
                Value::SmallHashInline(h) => Ok(h.len()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn hmget(
        &mut self,
        key: &[u8],
        fields: &[Vec<u8>],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        let borrowed: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        self.hmget_borrowed(key, &borrowed)
    }

    /// G4 (v1.25): borrowed-slice `HMGET`.
    pub fn hmget_borrowed(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(fields.iter().map(|_| None).collect()),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(fields.iter().map(|f| h.get(*f).cloned()).collect()),
                Value::SmallHashInline(h) => Ok(fields
                    .iter()
                    .map(|f| h.get(*f).map(<[u8]>::to_vec))
                    .collect()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `HGETALL` — flat `[field, value, field, value, ...]`.
    pub fn hgetall(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.hash_pairs(key)? {
            None => Ok(Vec::new()),
            Some(pairs) => {
                let mut out = Vec::with_capacity(pairs.len() * 2);
                for (f, v) in pairs {
                    out.push(f);
                    out.push(v);
                }
                Ok(out)
            }
        }
    }

    pub fn hkeys(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.keys().map(kevy_bytes::SmallBytes::to_vec).collect()),
                Value::SmallHashInline(h) => Ok(h.iter().map(|(f, _)| f.to_vec()).collect()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn hvals(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.live_entry(key) {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.values().cloned().collect()),
                Value::SmallHashInline(h) => Ok(h.iter().map(|(_, v)| v.to_vec()).collect()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `HDEL` — returns count removed; deletes the key if hash becomes empty.
    pub fn hdel(&mut self, key: &[u8], fields: &[Vec<u8>]) -> Result<usize, StoreError> {
        let borrowed: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        self.hdel_borrowed(key, &borrowed)
    }

    /// G4 (v1.25): borrowed-slice `HDEL`. A.8: encoding-aware.
    pub fn hdel_borrowed(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
    ) -> Result<usize, StoreError> {
        let now = now_ns();
        if !self.reap(key, now) {
            return Ok(0);
        }
        let (removed, delta, drop_key) = {
            let h_entry = self.map.get_mut(key).expect("live");
            match &mut h_entry.value {
                Value::Hash(h) => {
                    // G-A3: hoist Arc::make_mut OUT of the loop — done
                    // once per command instead of per-field.
                    let h = Arc::make_mut(h);
                    let mut r = 0usize;
                    let mut d: i64 = 0;
                    for f in fields {
                        if let Some(old_v) = h.remove(*f) {
                            r += 1;
                            let smb = SmallBytes::from_slice(f);
                            d -= hash_field_weight(&smb, old_v.len()) as i64;
                        }
                    }
                    let drop_now = h.is_empty();
                    (r, d, drop_now)
                }
                Value::SmallHashInline(h) => {
                    let mut r = 0usize;
                    for f in fields {
                        if h.try_remove(f) {
                            r += 1;
                        }
                    }
                    let drop_now = h.is_empty();
                    (r, 0i64, drop_now)
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

    /// `HINCRBYFLOAT` — atomic float increment of a hash field.
    /// Preserves TTL; errors with `NotFloat` if the field isn't a
    /// parseable float. Returns the post-increment value.
    pub fn hincrbyfloat(
        &mut self,
        key: &[u8],
        field: &[u8],
        delta: f64,
    ) -> Result<f64, StoreError> {
        let (next, weight_delta) = {
            let h = self.hash_mut(key, true)?.expect("created");
            let cur = match h.get(field) {
                Some(v) => parse_f64(v).ok_or(StoreError::NotFloat)?,
                None => 0.0,
            };
            let next = cur + delta;
            if !next.is_finite() {
                return Err(StoreError::NotFloat);
            }
            let new_bytes = format!("{next}").into_bytes();
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

    /// A.8 core: set one `(field, value)` pair, applying the
    /// encoding-switch. Returns the per-call outcome (whether added or
    /// updated, and whether it sits in the inline or heap variant).
    fn hset_one(
        &mut self,
        key: &[u8],
        field: &[u8],
        value: &[u8],
    ) -> Result<HsetOutcome, StoreError> {
        // Missing key — pick encoding by first pair size.
        if self.hash_value_for_set(key)?.is_none() {
            return Ok(self.hset_create(key, field, value));
        }
        let v = self.hash_value_for_set(key)?.expect("present and a hash");
        match v {
            Value::SmallHashInline(h) => match h.try_set(field, value) {
                HAddResult::Added => Ok(HsetOutcome::AddedInline),
                HAddResult::Updated => Ok(HsetOutcome::UpdatedInline),
                HAddResult::NoRoom => {
                    // Promote inline → Hash(Arc<HashData>), then set
                    // (handles the spilling pair).
                    let mut promoted = small_hash::promote(h);
                    let smb = SmallBytes::from_slice(field);
                    let new_w = hash_field_weight(&smb, value.len()) as i64;
                    let added = !promoted.contains_key(field);
                    let prior_v_len = promoted.get(field).map_or(0, Vec::len);
                    promoted.insert(smb, value.to_vec());
                    *v = Value::Hash(Arc::new(promoted));
                    self.reweigh_entry(key);
                    if added {
                        Ok(HsetOutcome::AddedHeap(new_w))
                    } else {
                        Ok(HsetOutcome::UpdatedHeap(value.len() as i64 - prior_v_len as i64))
                    }
                }
            },
            Value::Hash(h) => {
                let h = Arc::make_mut(h);
                let smb = SmallBytes::from_slice(field);
                let new_w = hash_field_weight(&smb, value.len()) as i64;
                let new_value_len = value.len();
                match h.insert(smb, value.to_vec()) {
                    None => Ok(HsetOutcome::AddedHeap(new_w)),
                    Some(old) => {
                        Ok(HsetOutcome::UpdatedHeap(new_value_len as i64 - old.len() as i64))
                    }
                }
            }
            _ => Err(StoreError::WrongType),
        }
    }

    /// Create a fresh entry for `key` holding one pair. Picks inline
    /// when both field + value fit, falls back to heap otherwise.
    fn hset_create(&mut self, key: &[u8], field: &[u8], value: &[u8]) -> HsetOutcome {
        if let Some(inline) = SmallHashData::with_one(field, value) {
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::SmallHashInline(inline), None),
            );
            // Insert already accounts via value.weight() == 0; per-pair
            // delta is zero in the caller (matches inline arm).
            HsetOutcome::AddedInline
        } else {
            let smb_f = SmallBytes::from_slice(field);
            let mut h = HashData::with_capacity(1);
            h.insert(smb_f, value.to_vec());
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::Hash(Arc::new(h)), None),
            );
            HsetOutcome::AddedInline
        }
    }

}

enum HsetOutcome {
    /// Field was new and lives in the inline variant (zero heap delta).
    AddedInline,
    /// Field existed in the inline variant (no count bump, no delta).
    UpdatedInline,
    /// Field was new in the heap variant; carries the new field's weight.
    AddedHeap(i64),
    /// Field existed in the heap variant; carries the value-length delta.
    UpdatedHeap(i64),
}
