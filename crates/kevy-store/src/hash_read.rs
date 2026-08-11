//! `Store` hash read commands — split from `hash.rs` when the SegHash
//! arms pushed it against the 500-LOC cap.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::{SmallBytes, Value};
use crate::{Store, StoreError};

/// `(field, value)` pairs collected off any hash encoding.
pub(crate) type FieldValuePairs = Vec<(Vec<u8>, Vec<u8>)>;

impl Store {
    /// Read the key's hash immutably (lazily expiring) — returns the
    /// pairs as a vector. None if absent. Internal helper for read-only
    /// paths; collects into a new Vec to avoid the encoding match dance
    /// at every callsite.
    pub(crate) fn hash_pairs(&mut self, key: &[u8]) -> Result<Option<FieldValuePairs>, StoreError> {
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(Some(
                    h.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect(),
                )),
                Value::SegHash(h) => Ok(Some(
                    h.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect(),
                )),
                Value::SmallHashInline(h) => Ok(Some(
                    h.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect(),
                )),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> Result<Option<&[u8]>, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(None),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.get(field).map(SmallBytes::as_slice)),
                Value::SegHash(h) => Ok(h.get(field).map(SmallBytes::as_slice)),
                Value::SmallHashInline(h) => Ok(h.get(field)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn hexists(&mut self, key: &[u8], field: &[u8]) -> Result<bool, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(false),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.contains_key(field)),
                Value::SegHash(h) => Ok(h.contains_key(field)),
                Value::SmallHashInline(h) => Ok(h.contains_key(field)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn hlen(&mut self, key: &[u8]) -> Result<usize, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(0),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.len()),
                Value::SegHash(h) => Ok(h.len()),
                Value::SmallHashInline(h) => Ok(h.len()),
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
                Value::SmallHashInline(h) => Ok(fields
                    .iter()
                    .map(|f| h.get(f).map(<[u8]>::to_vec))
                    .collect()),
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

    pub fn hkeys(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.keys().map(kevy_bytes::SmallBytes::to_vec).collect()),
                Value::SegHash(h) => Ok(h.keys().map(kevy_bytes::SmallBytes::to_vec).collect()),
                Value::SmallHashInline(h) => Ok(h.iter().map(|(f, _)| f.to_vec()).collect()),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    pub fn hvals(&mut self, key: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
        self.purge_hash_ttl(key);
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(Vec::new()),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.values().map(SmallBytes::to_vec).collect()),
                Value::SegHash(h) => Ok(h.values().map(SmallBytes::to_vec).collect()),
                Value::SmallHashInline(h) => Ok(h.iter().map(|(_, v)| v.to_vec()).collect()),
                _ => Err(StoreError::WrongType),
            },
        }
    }
}
