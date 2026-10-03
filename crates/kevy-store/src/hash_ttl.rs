//! Per-field hash TTLs (`HEXPIRE` / `HPEXPIRE` / `HPEXPIREAT` /
//! `HTTL` / `HPERSIST`, Redis 7.4 semantics).
//!
//! Storage: a store-level sidecar `hfttl: key → (field → absolute
//! unix-ms deadline)` holding ONLY keys that have at least one
//! field TTL — a store that never uses the feature pays one
//! `is_empty()` branch per hash access and nothing else.
//!
//! Enforcement follows the key-TTL discipline exactly:
//! - **lazy on access**: every hash op calls [`Store::purge_hash_ttl`]
//!   first, which removes expired fields from the hash (and the
//!   sidecar) — mutating-on-read like `live_entry`. No AOF frames are
//!   written for lazy purges: the `HPEXPIREAT` frame that created the
//!   deadline is already in the log, so a replay reconstructs the
//!   sidecar and purges identically (deterministic).
//! - **actively by the reaper**: [`Store::tick_hash_ttl`] sweeps due
//!   fields and reports what it removed so the caller can log the
//!   `HDEL` effect (server tick / embedded reaper).
//! - **cleared on overwrite**: `HSET`/`HINCRBY*` on a field discards
//!   that field's TTL (Redis 7.4 behavior) via
//!   [`Store::clear_hash_field_ttls`]; whole-key removal drops the
//!   sidecar entry in `remove_entry`.

// The discarded value is the operation's own count — how many fields
// went, how many members landed — and the caller returns its own.
#![expect(
    clippy::let_underscore_must_use,
    reason = "the discarded value is a count, not an error report"
)]

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::{SmallBytes, Store, StoreError, Value, now_unix_ms};

/// Per-field reply codes for `HEXPIRE`-family calls (Redis 7.4):
/// `-2` key or field missing, `0` condition (NX/XX/GT/LT) not met,
/// `1` deadline set, `2` field deleted (deadline already due).
///
/// ```
/// use kevy_store::{HExpireCode, HExpireCond, Store};
/// let mut s = Store::new();
/// s.hset(b"h", &[(b"a".as_slice(), b"1".as_slice()), (b"b", b"2")])?;
/// let far = kevy_store::now_unix_ms() + 60_000;
/// let codes: Vec<HExpireCode> =
///     s.hexpire_at(b"h", &[b"a".as_slice(), b"zz"], far, HExpireCond::Always)?;
/// assert_eq!(codes, [1, -2]);
/// assert_eq!(s.hexpire_at(b"h", &[b"b".as_slice()], 1, HExpireCond::Always)?, [2]);
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
pub type HExpireCode = i8;

/// Condition flags for `HEXPIRE` (`NX`/`XX`/`GT`/`LT`; at most one).
///
/// ```
/// assert_eq!(kevy_store::HExpireCond::default(), kevy_store::HExpireCond::Always);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum HExpireCond {
    /// Unconditional.
    ///
    /// ```
    /// use kevy_store::{HExpireCond, Store};
    /// let mut s = Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let t = kevy_store::now_unix_ms() + 60_000;
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t, HExpireCond::Always)?, [1]);
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t + 1, HExpireCond::Always)?, [1]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    #[default]
    Always,
    /// Only when the field has no TTL.
    ///
    /// ```
    /// use kevy_store::{HExpireCond, Store};
    /// let mut s = Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let t = kevy_store::now_unix_ms() + 60_000;
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t, HExpireCond::Nx)?, [1]);
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t, HExpireCond::Nx)?, [0]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Nx,
    /// Only when the field already has a TTL.
    ///
    /// ```
    /// use kevy_store::{HExpireCond, Store};
    /// let mut s = Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let t = kevy_store::now_unix_ms() + 60_000;
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t, HExpireCond::Xx)?, [0]);
    /// s.hexpire_at(b"h", &[b"f".as_slice()], t, HExpireCond::Always)?;
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t, HExpireCond::Xx)?, [1]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Xx,
    /// Only when the new deadline is later than the current one
    /// (no TTL counts as infinitely late — GT never replaces it).
    ///
    /// ```
    /// use kevy_store::{HExpireCond, Store};
    /// let mut s = Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let t = kevy_store::now_unix_ms() + 60_000;
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t, HExpireCond::Gt)?, [0]);
    /// s.hexpire_at(b"h", &[b"f".as_slice()], t, HExpireCond::Always)?;
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t + 1, HExpireCond::Gt)?, [1]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Gt,
    /// Only when the new deadline is earlier (no TTL = always).
    ///
    /// ```
    /// use kevy_store::{HExpireCond, Store};
    /// let mut s = Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let t = kevy_store::now_unix_ms() + 60_000;
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t, HExpireCond::Lt)?, [1]);
    /// assert_eq!(s.hexpire_at(b"h", &[b"f".as_slice()], t + 1, HExpireCond::Lt)?, [0]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Lt,
}

impl HExpireCond {
    /// The command keyword for this condition (`NX` / `XX` / `GT` /
    /// `LT`), or `None` for the unconditional form, which has none.
    ///
    /// ```
    /// use kevy_store::HExpireCond;
    /// assert_eq!(HExpireCond::Gt.keyword(), Some("GT"));
    /// assert_eq!(HExpireCond::Always.keyword(), None);
    /// ```
    pub fn keyword(self) -> Option<&'static str> {
        match self {
            Self::Always => None,
            Self::Nx => Some("NX"),
            Self::Xx => Some("XX"),
            Self::Gt => Some("GT"),
            Self::Lt => Some("LT"),
        }
    }
}

impl Store {
    /// Does this hash field exist (ignoring TTL state)?
    pub(crate) fn hash_has_field(&mut self, key: &[u8], field: &[u8]) -> Result<bool, StoreError> {
        match self.tier_serve(key, crate::value::COLD_TAG_HASH)? {
            None => Ok(false),
            Some(e) => match &e.value {
                Value::Hash(h) => Ok(h.get(field).is_some()),
                Value::SegHash(h) => Ok(h.get(field).is_some()),
                Value::SmallHashInline(h) => Ok(h.get(field).is_some()),
                Value::PackedRow(r) => Ok(r.has_named(field)),
                _ => Err(StoreError::WrongType),
            },
        }
    }

    /// Set per-field deadlines (absolute unix-ms). One code per field,
    /// request order. Due-or-past deadlines delete the field
    /// immediately (code `2`, Redis semantics).
    pub fn hexpire_at(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
        deadline_ms: u64,
        cond: HExpireCond,
    ) -> Result<Vec<HExpireCode>, StoreError> {
        self.hexpire_as_of(key, fields, deadline_ms, now_unix_ms(), cond)
    }

    /// [`Self::hexpire_at`] with "now" given: the instant a relative TTL
    /// was added to. A deadline at or before it deletes the field, so a
    /// positive TTL never does, however long after `now_ms` this runs.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let then = kevy_store::now_unix_ms() - 5;
    /// // one millisecond after a "now" already passed still sets the TTL
    /// let codes = s.hexpire_as_of(b"h", &[b"f"], then + 1, then, kevy_store::HExpireCond::Always)?;
    /// assert_eq!(codes, [1]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn hexpire_as_of(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
        deadline_ms: u64,
        now: u64,
        cond: HExpireCond,
    ) -> Result<Vec<HExpireCode>, StoreError> {
        let mut codes = Vec::with_capacity(fields.len());
        self.hexpire_as_of_each(key, fields, deadline_ms, now, cond, |c| codes.push(c))?;
        Ok(codes)
    }

    /// [`Self::hexpire_as_of`], each field's code handed to `f` in request
    /// order. An error comes before any code.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let now = kevy_store::now_unix_ms();
    /// let mut codes = Vec::new();
    /// s.hexpire_as_of_each(b"h", &[b"f", b"x"], now + 9, now, kevy_store::HExpireCond::Always, |c| codes.push(c))?;
    /// assert_eq!(codes, [1, -2]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn hexpire_as_of_each(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
        deadline_ms: u64,
        now: u64,
        cond: HExpireCond,
        mut f: impl FnMut(HExpireCode),
    ) -> Result<(), StoreError> {
        self.purge_hash_ttl(key);
        for field in fields {
            if !self.hash_has_field(key, field)? {
                f(-2);
                continue;
            }
            f(self.hexpire_field(key, field, deadline_ms, now, cond)?);
        }
        self.prune_hfttl_key(key);
        Ok(())
    }

    /// One present field of [`Self::hexpire_as_of_each`].
    fn hexpire_field(
        &mut self,
        key: &[u8],
        field: &[u8],
        deadline_ms: u64,
        now: u64,
        cond: HExpireCond,
    ) -> Result<HExpireCode, StoreError> {
        let current = self.hfttl.get(key).and_then(|m| m.get(field)).copied();
        let pass = match cond {
            HExpireCond::Always => true,
            HExpireCond::Nx => current.is_none(),
            HExpireCond::Xx => current.is_some(),
            HExpireCond::Gt => current.is_some_and(|c| deadline_ms > c),
            HExpireCond::Lt => current.is_none_or(|c| deadline_ms < c),
        };
        if !pass {
            return Ok(0);
        }
        if deadline_ms <= now {
            if let Some(m) = self.hfttl.get_mut(key) {
                m.remove(field);
            }
            self.hdel(key, &[field])?;
            return Ok(2);
        }
        match hfttl_slot(&mut self.hfttl, key).get_mut(field) {
            Some(d) => *d = deadline_ms,
            None => {
                hfttl_slot(&mut self.hfttl, key).insert(SmallBytes::from_slice(field), deadline_ms);
            }
        }
        Ok(1)
    }

    /// Clear per-field TTLs: `-2` missing, `-1` had no TTL, `1` cleared.
    pub fn hpersist(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
    ) -> Result<Vec<HExpireCode>, StoreError> {
        let mut out = Vec::with_capacity(fields.len());
        self.hpersist_each(key, fields, |c| out.push(c))?;
        Ok(out)
    }

    /// [`Self::hpersist`], each field's code handed to `f` in request
    /// order. An error comes before any code.
    ///
    /// ```
    /// let mut s = kevy_store::Store::new();
    /// s.hset(b"h", &[(b"f".as_slice(), b"v".as_slice())])?;
    /// let mut codes = Vec::new();
    /// s.hpersist_each(b"h", &[b"f"], |c| codes.push(c))?;
    /// assert_eq!(codes, [-1]);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub fn hpersist_each(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
        mut f: impl FnMut(HExpireCode),
    ) -> Result<(), StoreError> {
        self.purge_hash_ttl(key);
        for field in fields {
            if !self.hash_has_field(key, field)? {
                f(-2);
                continue;
            }
            let had = self.hfttl.get_mut(key).and_then(|m| m.remove(*field)).is_some();
            f(if had { 1 } else { -1 });
        }
        self.prune_hfttl_key(key);
        Ok(())
    }

    /// Lazy enforcement hook — call at the top of every hash op.
    /// Removes expired fields from the hash + sidecar. One `is_empty`
    /// branch when the feature is unused.
    pub(crate) fn purge_hash_ttl(&mut self, key: &[u8]) {
        if self.hfttl.is_empty() {
            return;
        }
        let now = now_unix_ms();
        let due: Vec<Vec<u8>> = match self.hfttl.get(key) {
            None => return,
            Some(m) => m.iter().filter(|(_, d)| **d <= now).map(|(f, _)| f.to_vec()).collect(),
        };
        if due.is_empty() {
            return;
        }
        // Sidecar first: `hdel` re-enters this fn, and a clean sidecar
        // makes the re-entry a no-op (no recursion).
        if let Some(m) = self.hfttl.get_mut(key) {
            for f in &due {
                m.remove(f.as_slice());
            }
        }
        self.prune_hfttl_key(key);
        let due_refs: Vec<&[u8]> = due.iter().map(Vec::as_slice).collect();
        let _ = self.hdel(key, &due_refs);
    }

    /// Overwrite hook — `HSET`/`HINCRBY*` on a field discards its TTL.
    pub(crate) fn clear_hash_field_ttls(&mut self, key: &[u8], fields: &[&[u8]]) {
        if self.hfttl.is_empty() {
            return;
        }
        if let Some(m) = self.hfttl.get_mut(key) {
            for f in fields {
                m.remove(*f);
            }
        }
        self.prune_hfttl_key(key);
    }

    /// Whole-key hook — key removed/overwritten wholesale.
    pub(crate) fn clear_hash_key_ttls(&mut self, key: &[u8]) {
        if self.hfttl.is_empty() {
            return;
        }
        self.hfttl.remove(key);
    }

    fn prune_hfttl_key(&mut self, key: &[u8]) {
        if self.hfttl.get(key).is_some_and(kevy_map_is_empty) {
            self.hfttl.remove(key);
        }
    }

    /// Reaper sweep: remove every due field store-wide; returns
    /// `(key, removed fields)` pairs so the caller logs `HDEL` effects.
    pub fn tick_hash_ttl(&mut self, max_keys: usize) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
        if self.hfttl.is_empty() {
            return Vec::new();
        }
        let now = now_unix_ms();
        let candidates: Vec<Vec<u8>> = self
            .hfttl
            .iter()
            .filter(|(_, m)| m.iter().any(|(_, d)| *d <= now))
            .take(max_keys)
            .map(|(k, _)| k.to_vec())
            .collect();
        let mut out = Vec::with_capacity(candidates.len());
        for k in candidates {
            let due: Vec<Vec<u8>> = self
                .hfttl
                .get(k.as_slice())
                .map(|m| m.iter().filter(|(_, d)| **d <= now).map(|(f, _)| f.to_vec()).collect())
                .unwrap_or_default();
            if due.is_empty() {
                continue;
            }
            if let Some(m) = self.hfttl.get_mut(k.as_slice()) {
                for f in &due {
                    m.remove(f.as_slice());
                }
            }
            self.prune_hfttl_key(&k);
            let due_refs: Vec<&[u8]> = due.iter().map(Vec::as_slice).collect();
            let _ = self.hdel(&k, &due_refs);
            out.push((k, due));
        }
        out
    }

    /// Snapshot loader hook: restore one field TTL (deadlines already
    /// absolute unix-ms; past deadlines simply purge on first access).
    pub fn load_hash_field_ttl(&mut self, key: &[u8], field: &[u8], deadline_ms: u64) {
        hfttl_slot(&mut self.hfttl, key).insert(SmallBytes::from_slice(field), deadline_ms);
    }

    /// Snapshot support: visit every live (key, field, deadline_ms).
    pub fn hash_ttl_each<F: FnMut(&[u8], &[u8], u64)>(&self, mut f: F) {
        for (k, m) in self.hfttl.iter() {
            for (field, &d) in m.iter() {
                f(k.as_slice(), field.as_slice(), d);
            }
        }
    }
}

fn kevy_map_is_empty(m: &crate::KevyMap<SmallBytes, u64>) -> bool {
    m.iter().next().is_none()
}

/// `entry().or_default()` over both side-map backends — `KevyMap` (the
/// `no_std` arm) has no entry API, so that arm inserts-if-absent and
/// re-probes.
fn hfttl_slot<'a>(
    hfttl: &'a mut crate::SideMap<SmallBytes, kevy_map::KevyMap<SmallBytes, u64>>,
    key: &[u8],
) -> &'a mut kevy_map::KevyMap<SmallBytes, u64> {
    #[cfg(feature = "std")]
    {
        hfttl.entry(SmallBytes::from_slice(key)).or_default()
    }
    #[cfg(not(feature = "std"))]
    {
        if hfttl.get(key).is_none() {
            hfttl.insert(SmallBytes::from_slice(key), kevy_map::KevyMap::default());
        }
        hfttl.get_mut(key).expect("inserted above")
    }
}

#[cfg(test)]
#[path = "tests_hash_ttl.rs"]
mod tests;
