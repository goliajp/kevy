//! String `SET` variants (`SETNX`, `APPEND`, `STRLEN`), hash
//! conditional set (`HSETNX`), decrement helpers (`DECR`, `DECRBY`,
//! `INCRBYFLOAT`), and the seconds-precision TTL accessor
//! (`ttl_secs`).

use std::io;

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Store, commit_write, store_err};

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> { Ok(()) }

impl Store {
    // ---- string SET variants ----------------------------------------

    /// `SETNX key value` — set only if the key does not exist.
    /// Returns `true` when the SET succeeded; `false` when it was
    /// vetoed by an existing value.
    pub fn setnx(&self, key: &[u8], value: &[u8]) -> io::Result<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let ok = g.store.set(key, value.to_vec(), None, /*nx=*/ true, /*xx=*/ false);
        if ok {
            commit_write(&mut g, &[b"SET", key, value, b"NX"])?;
        }
        Ok(ok)
    }

    /// `INCRBYFLOAT key delta` — atomic float increment of a string
    /// value. Returns the post-increment value parsed as f64.
    pub fn incrbyfloat(&self, key: &[u8], delta: f64) -> io::Result<f64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_bytes = g.store.incr_by_float(key, delta).map_err(store_err)?;
        let delta_str = format!("{delta}");
        commit_write(&mut g, &[b"INCRBYFLOAT", key, delta_str.as_bytes()])?;
        std::str::from_utf8(&new_bytes)
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .ok_or_else(|| io::Error::other("incrbyfloat result not parseable"))
    }

    /// `DECR key` — atomic decrement by 1.
    pub fn decr(&self, key: &[u8]) -> io::Result<i64> {
        self.incr_by(key, -1)
    }

    /// `DECRBY key delta` — atomic decrement by `delta`.
    pub fn decrby(&self, key: &[u8], delta: i64) -> io::Result<i64> {
        self.incr_by(key, delta.checked_neg().unwrap_or(i64::MIN.saturating_add(1)))
    }

    /// `STRLEN key` — length of the string value at `key`; 0 if
    /// absent. Errors on wrong type.
    pub fn strlen(&self, key: &[u8]) -> io::Result<usize> {
        self.wshard(key).store.strlen(key).map_err(store_err)
    }

    /// `APPEND key data` — append `data` to the string at `key`.
    /// Creates the key if absent. Returns the new total length.
    pub fn append(&self, key: &[u8], data: &[u8]) -> io::Result<usize> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_len = g.store.append(key, data).map_err(store_err)?;
        commit_write(&mut g, &[b"APPEND", key, data])?;
        Ok(new_len)
    }

    // ---- hash conditional set ---------------------------------------

    /// `HSETNX key field value` — set the hash field only if it
    /// does not already exist. Returns `true` when set; `false`
    /// when the field existed.
    pub fn hsetnx(&self, key: &[u8], field: &[u8], value: &[u8]) -> io::Result<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let ok = g.store.hsetnx(key, field, value).map_err(store_err)?;
        if ok {
            commit_write(&mut g, &[b"HSETNX", key, field, value])?;
        }
        Ok(ok)
    }

    // ---- TTL units --------------------------------------------------

    /// `TTL key` — TTL in **seconds** (truncated from ms). `-1`
    /// when the key has no TTL; `-2` when absent. Matches Redis
    /// wire semantics for the integer reply.
    pub fn ttl_secs(&self, key: &[u8]) -> i64 {
        let ms = self.ttl_ms(key);
        if ms <= 0 { ms } else { ms / 1000 }
    }
}
