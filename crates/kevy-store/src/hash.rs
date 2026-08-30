//! `Store` hash write commands. Reads live in `hash_read.rs`.
//!
//! Three encodings, promoted in order of size: `SmallHashInline`
//! (couple of tiny pairs, in the Value body) → `Hash(Arc<KevyMap>)`
//! (flat heap) → `SegHash` (bucket-sharded COW past
//! [`crate::seg_map::HS_PROMOTE`] fields — a write under a live
//! snapshot view clones one bucket, not the whole value).

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::seg_map::{HS_PROMOTE, SegMap};
use crate::small_hash::{self, AddResult as HAddResult, SmallHashData};
use crate::util::{parse_f64, parse_i64};
use crate::value::{HashData, SmallBytes, Value, hash_field_weight};
use crate::{Entry, Store, StoreError, now_ns};
use alloc::sync::Arc;

/// A mutable borrow of either heap hash encoding — lets the
/// read-modify-write entry points (`hincrby` / `hincrbyfloat`) stay
/// encoding-blind: both arms expose the same `get`/`insert` shape.
pub(crate) enum HashRefMut<'a> {
    Flat(&'a mut HashData),
    Seg(&'a mut SegMap<SmallBytes>),
}

impl HashRefMut<'_> {
    fn get(&self, field: &[u8]) -> Option<&SmallBytes> {
        match self {
            Self::Flat(h) => h.get(field),
            Self::Seg(h) => h.get(field),
        }
    }
    fn insert(&mut self, field: SmallBytes, value: SmallBytes) -> Option<SmallBytes> {
        match self {
            Self::Flat(h) => h.insert(field, value),
            Self::Seg(h) => h.insert(field, value),
        }
    }
}

impl Store {
    // ---- hashes --------------------------------------------------------

    /// Borrow the key's hash mutably, optionally creating it. `Ok(None)`
    /// means the key is absent and `create` was false. Promotes inline →
    /// flat, and flat → sharded at the threshold, so HINCRBY-only
    /// workloads cross the segmentation boundary too.
    fn hash_mut(&mut self, key: &[u8], create: bool) -> Result<Option<HashRefMut<'_>>, StoreError> {
        self.tier_resolve(key, crate::value::COLD_TAG_HASH)?;
        self.unpack_row(key);
        if self.live_entry_mut(key).is_none() {
            if !create {
                return Ok(None);
            }
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::Hash(Arc::default()), None),
            );
        }
        // A.8: detect encoding first (independent borrow), upgrade
        // out-of-scope of the &mut, then re-borrow — the borrow checker
        // rejects the in-place match.
        let needs = match self.map.get(key).map(|e| &e.value) {
            Some(Value::SmallHashInline(_)) => true,
            Some(Value::Hash(h)) => h.len() >= HS_PROMOTE,
            _ => false,
        };
        if needs {
            self.promote_hash_encoding(key);
        }
        match &mut self.map.get_mut(key).expect("present").value {
            Value::Hash(h) => Ok(Some(HashRefMut::Flat(Arc::make_mut(h)))),
            Value::SegHash(h) => Ok(Some(HashRefMut::Seg(Arc::make_mut(h)))),
            _ => Err(StoreError::WrongType),
        }
    }

    /// One promotion step: inline → flat, or flat-at-threshold →
    /// sharded. Reweighs the entry (the encoding switch changes the
    /// overhead model).
    fn promote_hash_encoding(&mut self, key: &[u8]) {
        let Some(e) = self.map.get_mut(key) else { return };
        match &mut e.value {
            Value::SmallHashInline(s) => {
                e.value = Value::Hash(Arc::new(small_hash::promote(s)));
            }
            Value::Hash(h) => {
                let flat = Arc::try_unwrap(core::mem::take(h)).unwrap_or_else(|a| (*a).clone());
                e.value = Value::SegHash(Arc::new(SegMap::from_flat(flat)));
            }
            _ => return,
        }
        self.reweigh_entry(key);
    }

    /// A.8: read the key's hash slot for HSET. `WrongType` if the entry
    /// is not a hash. Returns `None` when the key is absent.
    fn hash_value_for_set(&mut self, key: &[u8]) -> Result<Option<&mut Value>, StoreError> {
        self.tier_resolve(key, crate::value::COLD_TAG_HASH)?;
        match self.live_entry_mut(key) {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(_)
                | Value::SegHash(_)
                | Value::SmallHashInline(_)
                | Value::PackedRow(_) => Ok(Some(&mut e.value)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `HSET` into a declared row.
    ///
    /// Three ways out, and every one keeps the data: same width overwrites in
    /// place, a different width rebuilds the buffer, and anything the packed
    /// form cannot hold — a field the table never declared, or a payload past
    /// what u16 offsets address — leaves the packed form for the general one
    /// with every value intact. None of them is an error: a packed row is a
    /// size class and a declaration, not a type.
    fn hset_packed(
        &mut self,
        key: &[u8],
        field: &[u8],
        value: &[u8],
    ) -> Result<HsetOutcome, StoreError> {
        let v = self.hash_value_for_set(key)?.expect("present and packed");
        let Value::PackedRow(r) = v else { return Err(StoreError::WrongType) };
        let slot = r.names().iter().position(|c| c == field);
        let existed = slot.is_some_and(|i| r.has(i));
        let rebuilt = match slot {
            Some(i) if r.set_same_width(i, value) => Some(()),
            Some(i) => r.with_column(i, Some(value)).map(|next| {
                *r = next;
            }),
            None => None,
        };
        if rebuilt.is_none() {
            return self.unpack_then_set(key, field, value);
        }
        self.reweigh_entry(key);
        Ok(if existed { HsetOutcome::UpdatedInline } else { HsetOutcome::AddedInline })
    }

    /// Leave the packed form for the general one, then apply the write.
    fn unpack_then_set(
        &mut self,
        key: &[u8],
        field: &[u8],
        value: &[u8],
    ) -> Result<HsetOutcome, StoreError> {
        self.unpack_row(key);
        let v = self.hash_value_for_set(key)?.expect("present");
        let Value::Hash(h) = v else { return Err(StoreError::WrongType) };
        let outcome = heap_hash_set(HashRefMut::Flat(Arc::make_mut(h)), field, value);
        self.reweigh_entry(key);
        Ok(outcome)
    }

    /// Turn a packed row back into the general hash, in place. A no-op on
    /// every other value, including a hash that is already general.
    ///
    /// Every mutation that is not the packed form's own fast path goes
    /// through here first. The alternative — teaching each mutating verb to
    /// edit the packed buffer — is how the form came to answer WRONGTYPE
    /// from `HDEL` and `HINCRBYFLOAT`: those verbs never named the packed
    /// form, so a catch-all answered for them. One conversion in front of
    /// the mutation makes the general arms right for all of them, and the
    /// table's write hook packs the row again afterwards.
    pub(crate) fn unpack_row(&mut self, key: &[u8]) {
        let Some(e) = self.map.get_mut(key) else { return };
        let Value::PackedRow(r) = &e.value else { return };
        let mut flat = HashData::with_capacity(r.len().max(1));
        for (f, val) in r.fields() {
            flat.insert(SmallBytes::from_slice(f), SmallBytes::from_slice(val));
        }
        e.value = Value::Hash(Arc::new(flat));
        self.reweigh_entry(key);
    }

    /// `HSET` — returns the count of newly-added fields.
    pub fn hset(&mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> Result<usize, StoreError> {
        self.purge_hash_ttl(key);
        // Overwriting a field discards its TTL (Redis 7.4).
        if !self.hfttl.is_empty() {
            let fs: Vec<&[u8]> = pairs.iter().map(|(f, _)| *f).collect();
            self.clear_hash_field_ttls(key, &fs);
        }
        if pairs.is_empty() {
            return Ok(0);
        }
        let mut added = 0usize;
        let mut delta: i64 = 0;
        for (f, v) in pairs {
            match self.hset_one(key, f, v)? {
                HsetOutcome::AddedInline => added += 1,
                HsetOutcome::UpdatedInline => {}
                HsetOutcome::AddedHeap(w) => {
                    added += 1;
                    delta += w;
                }
                HsetOutcome::UpdatedHeap(d) => delta += d,
            }
        }
        self.account_delta(key, delta);
        Ok(added)
    }

    /// `HSETNX` — set only if the field is absent; returns whether set.
    pub fn hsetnx(&mut self, key: &[u8], field: &[u8], val: &[u8]) -> Result<bool, StoreError> {
        self.purge_hash_ttl(key);
        self.tier_resolve(key, crate::value::COLD_TAG_HASH)?;
        let exists = match self.live_entry(key) {
            None => false,
            Some(e) => match &e.value {
                Value::Hash(h) => h.contains_key(field),
                Value::SegHash(h) => h.contains_key(field),
                Value::SmallHashInline(h) => h.contains_key(field),
                Value::PackedRow(r) => r.has_named(field),
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

    /// `HDEL` — returns count removed; deletes the key if emptied.
    pub fn hdel(&mut self, key: &[u8], fields: &[&[u8]]) -> Result<usize, StoreError> {
        self.purge_hash_ttl(key);
        self.tier_resolve(key, crate::value::COLD_TAG_HASH)?;
        self.unpack_row(key);
        let now = now_ns();
        if !self.reap(key, now) {
            return Ok(0);
        }
        let (removed, delta, drop_key) = {
            let h_entry = self.map.get_mut(key).expect("live");
            match &mut h_entry.value {
                // G-A3: hoist Arc::make_mut OUT of the loop — done once
                // per command instead of per-field.
                Value::Hash(h) => heap_hash_del(HashRefMut::Flat(Arc::make_mut(h)), fields),
                Value::SegHash(h) => heap_hash_del(HashRefMut::Seg(Arc::make_mut(h)), fields),
                Value::SmallHashInline(h) => {
                    let mut r = 0usize;
                    for f in fields {
                        if h.try_remove(f) {
                            r += 1;
                        }
                    }
                    (r, 0i64, h.is_empty())
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
    pub fn hincrbyfloat(
        &mut self,
        key: &[u8],
        field: &[u8],
        delta: f64,
    ) -> Result<f64, StoreError> {
        self.purge_hash_ttl(key);
        self.clear_hash_field_ttls(key, &[field]);
        let (next, weight_delta) = {
            let mut h = self.hash_mut(key, true)?.expect("created");
            let cur = match h.get(field) {
                Some(v) => parse_f64(v.as_slice()).ok_or(StoreError::NotFloat)?,
                None => 0.0,
            };
            let next = cur + delta;
            if !next.is_finite() {
                return Err(StoreError::NotFloat);
            }
            let vb = SmallBytes::from_vec(format!("{next}").into_bytes());
            let smb = SmallBytes::from_slice(field);
            let new_field_w = hash_field_weight(&smb, vb.heap_bytes()) as i64;
            let new_value_heap = vb.heap_bytes() as i64;
            let wd = match h.insert(smb, vb) {
                None => new_field_w,
                Some(old) => new_value_heap - old.heap_bytes() as i64,
            };
            (next, wd)
        };
        self.account_delta(key, weight_delta);
        Ok(next)
    }

    /// `HINCRBY` — preserves TTL; errors if the field isn't an integer.
    pub fn hincrby(&mut self, key: &[u8], field: &[u8], delta: i64) -> Result<i64, StoreError> {
        self.purge_hash_ttl(key);
        self.clear_hash_field_ttls(key, &[field]);
        let (next, weight_delta) = {
            let mut h = self.hash_mut(key, true)?.expect("created");
            let cur = match h.get(field) {
                Some(v) => parse_i64(v.as_slice()).ok_or(StoreError::NotInteger)?,
                None => 0,
            };
            let next = cur.checked_add(delta).ok_or(StoreError::Overflow)?;
            let vb = SmallBytes::from_vec(next.to_string().into_bytes());
            let smb = SmallBytes::from_slice(field);
            let new_field_w = hash_field_weight(&smb, vb.heap_bytes()) as i64;
            let new_value_heap = vb.heap_bytes() as i64;
            let wd = match h.insert(smb, vb) {
                None => new_field_w,
                Some(old) => new_value_heap - old.heap_bytes() as i64,
            };
            (next, wd)
        };
        self.account_delta(key, weight_delta);
        Ok(next)
    }

    /// A.8 core: set one `(field, value)` pair, applying the
    /// encoding-switch.
    fn hset_one(
        &mut self,
        key: &[u8],
        field: &[u8],
        value: &[u8],
    ) -> Result<HsetOutcome, StoreError> {
        if self.hash_value_for_set(key)?.is_none() {
            return Ok(self.hset_create(key, field, value));
        }
        let v = self.hash_value_for_set(key)?.expect("present and a hash");
        match v {
            Value::SmallHashInline(h) => match h.try_set(field, value) {
                HAddResult::Added => Ok(HsetOutcome::AddedInline),
                HAddResult::Updated => Ok(HsetOutcome::UpdatedInline),
                HAddResult::NoRoom => {
                    let mut promoted = small_hash::promote(h);
                    let outcome = heap_hash_set(HashRefMut::Flat(&mut promoted), field, value);
                    *v = Value::Hash(Arc::new(promoted));
                    self.reweigh_entry(key);
                    Ok(outcome)
                }
            },
            Value::PackedRow(_) => self.hset_packed(key, field, value),
            // Flat hash at the threshold: shard, then set. One-time
            // O(HS_PROMOTE) re-bucket (or clone, if a view pins it now).
            Value::Hash(h) if h.len() >= HS_PROMOTE => {
                let flat = Arc::try_unwrap(core::mem::take(h)).unwrap_or_else(|a| (*a).clone());
                let mut seg = SegMap::from_flat(flat);
                let outcome = heap_hash_set(HashRefMut::Seg(&mut seg), field, value);
                *v = Value::SegHash(Arc::new(seg));
                self.reweigh_entry(key);
                // Reweighed from scratch — swallow the per-pair delta.
                Ok(match outcome {
                    HsetOutcome::AddedHeap(_) => HsetOutcome::AddedHeap(0),
                    other => other,
                })
            }
            Value::Hash(h) => Ok(heap_hash_set(HashRefMut::Flat(Arc::make_mut(h)), field, value)),
            Value::SegHash(h) => Ok(heap_hash_set(HashRefMut::Seg(Arc::make_mut(h)), field, value)),
            _ => Err(StoreError::WrongType),
        }
    }

    /// Create a fresh entry for `key` holding one pair.
    fn hset_create(&mut self, key: &[u8], field: &[u8], value: &[u8]) -> HsetOutcome {
        if let Some(inline) = SmallHashData::with_one(field, value) {
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::SmallHashInline(inline), None),
            );
            HsetOutcome::AddedInline
        } else {
            let smb_f = SmallBytes::from_slice(field);
            let mut h = HashData::with_capacity(1);
            h.insert(smb_f, SmallBytes::from_slice(value));
            self.insert_entry(
                SmallBytes::from_slice(key),
                Entry::new(Value::Hash(Arc::new(h)), None),
            );
            HsetOutcome::AddedInline
        }
    }
}

/// Set one `(field, value)` pair into a heap-backed hash (either
/// encoding), charging heap bytes only.
fn heap_hash_set(mut h: HashRefMut<'_>, field: &[u8], value: &[u8]) -> HsetOutcome {
    let smb = SmallBytes::from_slice(field);
    let vb = SmallBytes::from_slice(value);
    let new_value_heap = vb.heap_bytes() as i64;
    let new_w = hash_field_weight(&smb, vb.heap_bytes()) as i64;
    match h.insert(smb, vb) {
        None => HsetOutcome::AddedHeap(new_w),
        Some(old) => HsetOutcome::UpdatedHeap(new_value_heap - old.heap_bytes() as i64),
    }
}

/// The HDEL field loop over a heap-backed hash (either encoding).
/// Returns `(removed, weight_delta, now_empty)`.
fn heap_hash_del(mut h: HashRefMut<'_>, fields: &[&[u8]]) -> (usize, i64, bool) {
    let mut r = 0usize;
    let mut d: i64 = 0;
    for f in fields {
        let old = match &mut h {
            HashRefMut::Flat(m) => m.remove(*f),
            HashRefMut::Seg(m) => m.remove(f),
        };
        if let Some(old_v) = old {
            r += 1;
            let smb = SmallBytes::from_slice(f);
            d -= hash_field_weight(&smb, old_v.heap_bytes()) as i64;
        }
    }
    let empty = match &h {
        HashRefMut::Flat(m) => m.is_empty(),
        HashRefMut::Seg(m) => m.is_empty(),
    };
    (r, d, empty)
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
