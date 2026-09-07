//! `Store` hash read commands — split from `hash.rs` when the SegHash
//! arms pushed it against the 500-LOC cap.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::{SmallBytes, Value};
use crate::{Store, StoreError};

/// `(field, value)` pairs collected off any hash encoding.
/// Owned `(field, value)` pairs, as the hash readers hand them back.
///
/// ```
/// use kevy_store::{FieldValuePairs, Store};
///
/// let mut s = Store::new();
/// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())]).unwrap();
///
/// // Both halves are owned, so the pairs outlive the borrow of the store.
/// let pairs: FieldValuePairs = s.hrandfield(b"h", 1, true).unwrap();
/// assert_eq!(pairs, vec![(b"f".to_vec(), b"v".to_vec())]);
/// ```
pub type FieldValuePairs = Vec<(Vec<u8>, Vec<u8>)>;

impl Store {
    /// Read the key's hash immutably (lazily expiring) — returns the
    /// pairs as a vector. None if absent. Internal helper for read-only
    /// paths; collects into a new Vec to avoid the encoding match dance
    /// at every callsite.
    pub(crate) fn hash_pairs(&mut self, key: &[u8]) -> Result<Option<FieldValuePairs>, StoreError> {
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(h) => {
                    Ok(Some(h.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()))
                }
                Value::SegHash(h) => {
                    Ok(Some(h.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()))
                }
                Value::SmallHashInline(h) => {
                    Ok(Some(h.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()))
                }
                Value::PackedRow(r) => {
                    Ok(Some(r.fields().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()))
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// One field's value, borrowed from the store. `Ok(None)` for a
    /// missing key or a missing field — the two are indistinguishable to
    /// HGET by design; `Err` only when `key` holds something that is not a
    /// hash.
    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> Result<Option<&[u8]>, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.get(field).map(SmallBytes::as_slice)),
                Value::SegHash(h) => Ok(h.get(field).map(SmallBytes::as_slice)),
                Value::SmallHashInline(h) => Ok(h.get(field)),
                Value::PackedRow(r) => Ok(r.get_named(field)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Whether `field` is present. A missing key is `false`, not an
    /// error; a wrong-typed key is an error.
    pub fn hexists(&mut self, key: &[u8], field: &[u8]) -> Result<bool, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(false),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.contains_key(field)),
                Value::SegHash(h) => Ok(h.contains_key(field)),
                Value::SmallHashInline(h) => Ok(h.contains_key(field)),
                Value::PackedRow(r) => Ok(r.has_named(field)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Field count. A missing key is 0, matching HLEN.
    pub fn hlen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(0),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.len()),
                Value::SegHash(h) => Ok(h.len()),
                Value::SmallHashInline(h) => Ok(h.len()),
                Value::PackedRow(r) => Ok(r.len()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `HMGET` — one `Option` per requested field, in input order.
    pub fn hmget(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(fields.iter().map(|_| None).collect()),
            Some(e) => match &e.value {
                Value::Hash(h) => {
                    Ok(fields.iter().map(|f| h.get(*f).map(SmallBytes::to_vec)).collect())
                }
                Value::SegHash(h) => {
                    Ok(fields.iter().map(|f| h.get(f).map(SmallBytes::to_vec)).collect())
                }
                Value::SmallHashInline(h) => {
                    Ok(fields.iter().map(|f| h.get(f).map(<[u8]>::to_vec)).collect())
                }
                Value::PackedRow(r) => {
                    Ok(fields.iter().map(|f| r.get_named(f).map(<[u8]>::to_vec)).collect())
                }
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// `HGETALL` — flat `[field, value, field, value, ...]`.
    pub fn hgetall(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        self.purge_hash_ttl(key);
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

    /// `HRANDFIELD` — `count` distinct fields, or `count.abs()` fields with
    /// repeats allowed when `count` is negative, which is Redis's way of
    /// asking for a sample rather than a subset.
    ///
    /// Built on `hash_pairs` so it covers all four storage forms at once
    /// (Hash / SegHash / SmallHashInline / PackedRow) rather than growing a
    /// fourth near-copy of the same match. `with_values` decides whether the
    /// value rides along; the RESP3 reply nests the pairs and RESP2 flattens
    /// them, which is the caller's business, not this one's.
    /// ```
    /// use kevy_store::Store;
    ///
    /// let mut s = Store::new();
    /// s.hset(b"h", &[(b"f1".as_slice(), b"v1".as_slice()),
    ///                (b"f2".as_slice(), b"v2".as_slice())]).unwrap();
    ///
    /// // A positive count is distinct, and capped at what the hash holds.
    /// assert_eq!(s.hrandfield(b"h", 9, false).unwrap().len(), 2);
    ///
    /// // A negative count returns exactly |count|, repeats allowed — the
    /// // distinction Redis draws between a subset and a sample.
    /// assert_eq!(s.hrandfield(b"h", -5, false).unwrap().len(), 5);
    ///
    /// // `with_values` fills the second half of each pair; without it the
    /// // value is empty and only the field name means anything.
    /// let pairs = s.hrandfield(b"h", 2, true).unwrap();
    /// assert!(pairs.iter().all(|(f, v)| !f.is_empty() && !v.is_empty()));
    ///
    /// // A missing key is empty, not an error.
    /// assert!(s.hrandfield(b"absent", 3, false).unwrap().is_empty());
    /// ```
    pub fn hrandfield(
        &mut self,
        key: &[u8],
        count: i64,
        with_values: bool,
    ) -> Result<FieldValuePairs, StoreError> {
        self.purge_hash_ttl(key);
        let Some(pairs) = self.hash_pairs(key)? else {
            return Ok(Vec::new());
        };
        if pairs.is_empty() || count == 0 {
            return Ok(Vec::new());
        }
        let n = pairs.len();
        let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

        if count < 0 {
            // Repeats allowed: draw independently, so the result may name the
            // same field twice and is as long as asked for.
            let want = count.unsigned_abs() as usize;
            out.reserve(want.min(1 << 20));
            for _ in 0..want.min(1 << 20) {
                let i = (self.rng.next_u64() % n as u64) as usize;
                let (f, v) = &pairs[i];
                out.push((f.clone(), if with_values { v.clone() } else { Vec::new() }));
            }
            return Ok(out);
        }

        let want = (count as usize).min(n);
        let mut idx: Vec<usize> = (0..n).collect();
        // Partial Fisher-Yates: only the prefix we return needs to be shuffled.
        for i in 0..want {
            let j = i + (self.rng.next_u64() % (n - i) as u64) as usize;
            idx.swap(i, j);
        }
        out.reserve(want);
        for &i in &idx[..want] {
            let (f, v) = &pairs[i];
            out.push((f.clone(), if with_values { v.clone() } else { Vec::new() }));
        }
        Ok(out)
    }

    /// Every field name, copied out. Unordered: a hash has no field
    /// order to preserve, so two calls may differ in sequence.
    pub fn hkeys(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.keys().map(kevy_bytes::SmallBytes::to_vec).collect()),
                Value::SegHash(h) => Ok(h.keys().map(kevy_bytes::SmallBytes::to_vec).collect()),
                Value::SmallHashInline(h) => Ok(h.iter().map(|(f, _)| f.to_vec()).collect()),
                Value::PackedRow(r) => Ok(r.fields().map(|(f, _)| f.to_vec()).collect()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Every value, copied out, in the same unordered sequence `hkeys`
    /// would return its fields.
    pub fn hvals(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.values().map(SmallBytes::to_vec).collect()),
                Value::SegHash(h) => Ok(h.values().map(SmallBytes::to_vec).collect()),
                Value::SmallHashInline(h) => Ok(h.iter().map(|(_, v)| v.to_vec()).collect()),
                Value::PackedRow(r) => Ok(r.fields().map(|(_, v)| v.to_vec()).collect()),
                _ => Err(StoreError::WrongType),
            },
        }
    }
}
