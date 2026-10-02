//! Reading hash field deadlines: as stored, as time left, as the instant.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::{Store, StoreError, now_unix_ms};

impl Store {
    /// Each field's absolute deadline (unix ms) as stored, `None` for a
    /// field with no TTL. A read with no side effects: unlike [`Self::hpttl`]
    /// it purges nothing, so a deadline already passed is still reported,
    /// and removing that field stays the expiry sweep's job.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.hset(b"h", &[(b"f", b"v"), (b"g", b"w")]).unwrap();
    /// let at = kevy_store::now_unix_ms() + 60_000;
    /// s.hexpire_at(b"h", &[b"f"], at, kevy_store::HExpireCond::Always).unwrap();
    /// assert_eq!(s.hash_field_deadlines(b"h", &[b"f", b"g"]), [Some(at), None]);
    /// ```
    pub fn hash_field_deadlines(&self, key: &[u8], fields: &[&[u8]]) -> Vec<Option<u64>> {
        let per_key = self.hfttl.get(key);
        fields.iter().map(|f| per_key.and_then(|m| m.get(*f)).copied()).collect()
    }

    /// Remaining TTL per field: `-2` key/field missing, `-1` no TTL,
    /// else remaining ms.
    pub fn hpttl(&mut self, key: &[u8], fields: &[&[u8]]) -> Result<Vec<i64>, StoreError> {
        let now = now_unix_ms();
        self.per_field_deadline(key, fields, |d| d.saturating_sub(now) as i64)
    }

    /// The unix-ms deadline per field: `-2` key/field missing, `-1` no TTL.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// s.hexpire_at(b"h", &[b"f"], 4_102_444_800_999, kevy_store::HExpireCond::Always)?;
    /// assert_eq!(s.hexpire_time(b"h", &[b"f", b"x"])?, [4_102_444_800_999, -2]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn hexpire_time(&mut self, key: &[u8], fields: &[&[u8]]) -> Result<Vec<i64>, StoreError> {
        self.per_field_deadline(key, fields, |d| d as i64)
    }

    fn per_field_deadline(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
        shown: impl Fn(u64) -> i64,
    ) -> Result<Vec<i64>, StoreError> {
        self.purge_hash_ttl(key);
        let mut out = Vec::with_capacity(fields.len());
        for f in fields {
            if !self.hash_has_field(key, f)? {
                out.push(-2);
                continue;
            }
            out.push(self.hfttl.get(key).and_then(|m| m.get(*f)).map_or(-1, |&d| shown(d)));
        }
        Ok(out)
    }
}
