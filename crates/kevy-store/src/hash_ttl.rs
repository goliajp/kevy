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

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::{SmallBytes, Store, StoreError, Value, now_unix_ms};

/// Per-field reply codes for `HEXPIRE`-family calls (Redis 7.4):
/// `-2` key or field missing, `0` condition (NX/XX/GT/LT) not met,
/// `1` deadline set, `2` field deleted (deadline already due).
pub type HExpireCode = i8;

/// Condition flags for `HEXPIRE` (`NX`/`XX`/`GT`/`LT`; at most one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HExpireCond {
    /// Unconditional.
    #[default]
    Always,
    /// Only when the field has no TTL.
    Nx,
    /// Only when the field already has a TTL.
    Xx,
    /// Only when the new deadline is later than the current one
    /// (no TTL counts as infinitely late — GT never replaces it).
    Gt,
    /// Only when the new deadline is earlier (no TTL = always).
    Lt,
}

impl Store {
    /// Does this hash field exist (ignoring TTL state)?
    fn hash_has_field(&mut self, key: &[u8], field: &[u8]) -> Result<bool, StoreError> {
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
        self.purge_hash_ttl(key);
        let mut codes = Vec::with_capacity(fields.len());
        let now = now_unix_ms();
        for f in fields {
            if !self.hash_has_field(key, f)? {
                codes.push(-2);
                continue;
            }
            let current = self
                .hfttl
                .get(key)
                .and_then(|m| m.get(*f))
                .copied();
            let pass = match cond {
                HExpireCond::Always => true,
                HExpireCond::Nx => current.is_none(),
                HExpireCond::Xx => current.is_some(),
                HExpireCond::Gt => current.is_some_and(|c| deadline_ms > c),
                HExpireCond::Lt => current.is_none_or(|c| deadline_ms < c),
            };
            if !pass {
                codes.push(0);
                continue;
            }
            if deadline_ms <= now {
                if let Some(m) = self.hfttl.get_mut(key) {
                    m.remove(*f);
                }
                self.hdel(key, &[f])?;
                codes.push(2);
                continue;
            }
            hfttl_slot(&mut self.hfttl, key)
                .insert(SmallBytes::from_slice(f), deadline_ms);
            codes.push(1);
        }
        self.prune_hfttl_key(key);
        Ok(codes)
    }

    /// Remaining TTL per field: `-2` key/field missing, `-1` no TTL,
    /// else remaining ms.
    pub fn hpttl(&mut self, key: &[u8], fields: &[&[u8]]) -> Result<Vec<i64>, StoreError> {
        self.purge_hash_ttl(key);
        let now = now_unix_ms();
        let mut out = Vec::with_capacity(fields.len());
        for f in fields {
            if !self.hash_has_field(key, f)? {
                out.push(-2);
                continue;
            }
            match self.hfttl.get(key).and_then(|m| m.get(*f)) {
                Some(&d) => out.push(d.saturating_sub(now) as i64),
                None => out.push(-1),
            }
        }
        Ok(out)
    }

    /// Clear per-field TTLs: `-2` missing, `-1` had no TTL, `1` cleared.
    pub fn hpersist(&mut self, key: &[u8], fields: &[&[u8]]) -> Result<Vec<HExpireCode>, StoreError> {
        self.purge_hash_ttl(key);
        let mut out = Vec::with_capacity(fields.len());
        for f in fields {
            if !self.hash_has_field(key, f)? {
                out.push(-2);
                continue;
            }
            let had = self
                .hfttl
                .get_mut(key)
                .and_then(|m| m.remove(*f))
                .is_some();
            out.push(if had { 1 } else { -1 });
        }
        self.prune_hfttl_key(key);
        Ok(out)
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
            Some(m) => m
                .iter()
                .filter(|(_, d)| **d <= now)
                .map(|(f, _)| f.to_vec())
                .collect(),
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
                .map(|m| {
                    m.iter()
                        .filter(|(_, d)| **d <= now)
                        .map(|(f, _)| f.to_vec())
                        .collect()
                })
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
        hfttl_slot(&mut self.hfttl, key)
            .insert(SmallBytes::from_slice(field), deadline_ms);
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
mod tests {
    use super::*;

    fn h(s: &mut Store) {
        s.hset(
            b"h",
            &[(b"a".as_slice(), b"1".as_slice()), (b"b".as_slice(), b"2".as_slice())],
        )
        .unwrap();
    }

    #[test]
    fn hexpire_httl_hpersist_codes() {
        let mut s = Store::new();
        h(&mut s);
        let far = now_unix_ms() + 100_000;
        // set on a + missing field
        let codes = s
            .hexpire_at(b"h", &[b"a", b"nope"], far, HExpireCond::Always)
            .unwrap();
        assert_eq!(codes, vec![1, -2]);
        let ttls = s.hpttl(b"h", &[b"a", b"b", b"nope"]).unwrap();
        assert!(ttls[0] > 90_000 && ttls[0] <= 100_000);
        assert_eq!(&ttls[1..], &[-1, -2]);
        // NX refuses existing, XX refuses missing
        assert_eq!(
            s.hexpire_at(b"h", &[b"a"], far + 1, HExpireCond::Nx).unwrap(),
            vec![0]
        );
        assert_eq!(
            s.hexpire_at(b"h", &[b"b"], far, HExpireCond::Xx).unwrap(),
            vec![0]
        );
        // GT/LT
        assert_eq!(
            s.hexpire_at(b"h", &[b"a"], far + 500, HExpireCond::Gt).unwrap(),
            vec![1]
        );
        assert_eq!(
            s.hexpire_at(b"h", &[b"a"], far, HExpireCond::Gt).unwrap(),
            vec![0]
        );
        // persist
        assert_eq!(s.hpersist(b"h", &[b"a", b"b", b"nope"]).unwrap(), vec![1, -1, -2]);
        assert_eq!(s.hpttl(b"h", &[b"a"]).unwrap(), vec![-1]);
    }

    #[test]
    fn past_deadline_deletes_and_lazy_purge_enforces() {
        let mut s = Store::new();
        h(&mut s);
        // past deadline → immediate delete, code 2
        assert_eq!(
            s.hexpire_at(b"h", &[b"a"], 1, HExpireCond::Always).unwrap(),
            vec![2]
        );
        assert!(!s.hexists(b"h", b"a").unwrap());
        // near-future deadline → lazily gone after it passes
        let soon = now_unix_ms() + 30;
        s.hexpire_at(b"h", &[b"b"], soon, HExpireCond::Always).unwrap();
        std::thread::sleep(core::time::Duration::from_millis(50));
        assert!(!s.hexists(b"h", b"b").unwrap(), "lazy purge on access");
        // hash is now empty → hlen 0, sidecar pruned
        assert_eq!(s.hlen(b"h").unwrap(), 0);
        assert!(s.hfttl.is_empty());
    }

    #[test]
    fn overwrite_clears_ttl_and_reaper_reports() {
        let mut s = Store::new();
        h(&mut s);
        let soon = now_unix_ms() + 20;
        s.hexpire_at(b"h", &[b"a", b"b"], soon, HExpireCond::Always).unwrap();
        // overwrite a → its TTL is discarded (Redis 7.4)
        s.hset(b"h", &[(b"a".as_slice(), b"new".as_slice())]).unwrap();
        assert_eq!(s.hpttl(b"h", &[b"a"]).unwrap(), vec![-1]);
        std::thread::sleep(core::time::Duration::from_millis(40));
        // reaper sweeps b, reports the removal for effect logging
        let swept = s.tick_hash_ttl(100);
        assert_eq!(swept, vec![(b"h".to_vec(), vec![b"b".to_vec()])]);
        assert!(s.hexists(b"h", b"a").unwrap(), "overwritten field survived");
        // whole-key delete drops the sidecar
        let far = now_unix_ms() + 100_000;
        s.hexpire_at(b"h", &[b"a"], far, HExpireCond::Always).unwrap();
        s.del(&[b"h".as_slice()]);
        assert!(s.hfttl.is_empty());
    }
}
