//! Embedded hash field-TTL facades (Redis 7.4 family).
//!
//! AOF: deadline writes log the canonical absolute `HPEXPIREAT`
//! frame regardless of which facade set them (relative forms convert
//! before logging — replay can't re-anchor); `hpersist` logs itself.

use crate::KevyResult;
use std::time::Duration;

use kevy_store::{HExpireCode, HExpireCond, now_unix_ms};

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Store, commit_write, store_err};

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> KevyResult<()> {
    Ok(())
}

impl Store {
    /// `HEXPIRE` — set a relative per-field TTL. One Redis code per
    /// field (`-2` missing, `0` condition failed, `1` set, `2` deleted).
    pub fn hexpire(
        &self,
        key: &[u8],
        fields: &[&[u8]],
        ttl: Duration,
        cond: HExpireCond,
    ) -> KevyResult<Vec<HExpireCode>> {
        let deadline = now_unix_ms().saturating_add(ttl.as_millis() as u64);
        self.hpexpire_at(key, fields, deadline, cond)
    }

    /// `HPEXPIREAT` — absolute unix-ms per-field deadline (the
    /// canonical form; also what the AOF carries).
    pub fn hpexpire_at(
        &self,
        key: &[u8],
        fields: &[&[u8]],
        deadline_ms: u64,
        cond: HExpireCond,
    ) -> KevyResult<Vec<HExpireCode>> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let codes = g.store.hexpire_at(key, fields, deadline_ms, cond).map_err(store_err)?;
        // Log only when something changed (set or immediate-delete).
        if codes.iter().any(|&c| c == 1 || c == 2) {
            let ms = deadline_ms.to_string();
            let n = fields.len().to_string();
            let mut argv: Vec<&[u8]> = Vec::with_capacity(5 + fields.len());
            argv.push(b"HPEXPIREAT");
            argv.push(key);
            argv.push(ms.as_bytes());
            argv.push(b"FIELDS");
            argv.push(n.as_bytes());
            argv.extend(fields.iter().copied());
            commit_write(&mut g, &argv)?;
        }
        Ok(codes)
    }

    /// `HTTL` — remaining ms per field (`-2` missing, `-1` no TTL).
    pub fn httl(&self, key: &[u8], fields: &[&[u8]]) -> KevyResult<Vec<i64>> {
        let mut g = self.wshard(key);
        g.store.httl(key, fields).map_err(store_err)
    }

    /// `HPERSIST` — clear per-field TTLs (`-2` missing, `-1` had no
    /// TTL, `1` cleared).
    pub fn hpersist(&self, key: &[u8], fields: &[&[u8]]) -> KevyResult<Vec<HExpireCode>> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let codes = g.store.hpersist(key, fields).map_err(store_err)?;
        if codes.contains(&1) {
            let n = fields.len().to_string();
            let mut argv: Vec<&[u8]> = Vec::with_capacity(4 + fields.len());
            argv.push(b"HPERSIST");
            argv.push(key);
            argv.push(b"FIELDS");
            argv.push(n.as_bytes());
            argv.extend(fields.iter().copied());
            commit_write(&mut g, &argv)?;
        }
        Ok(codes)
    }
}
