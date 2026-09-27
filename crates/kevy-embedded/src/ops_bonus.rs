//! String `SET` variants (`SETNX`, SET with NX/XX/TTL, `APPEND`, `STRLEN`), hash
//! conditional set (`HSETNX`), decrement helpers (`DECR`, `DECRBY`,
//! `INCRBYFLOAT`), and the seconds-precision TTL accessor
//! (`ttl_secs`).

use std::time::Duration;

use crate::{KevyError, KevyResult};

use crate::store::ensure_writable;
use crate::store::{Store, commit_write, store_err};
use crate::store_glue::commit_deadline;

impl Store {
    // ---- string SET variants ----------------------------------------

    /// `SETNX key value` — set only if the key does not exist.
    /// Returns `true` when the SET succeeded; `false` when it was
    /// vetoed by an existing value.
    pub fn setnx(&self, key: &[u8], value: &[u8]) -> KevyResult<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let ok = g.store.set(key, value.to_vec(), None, /*nx=*/ true, /*xx=*/ false);
        if ok {
            commit_write(&mut g, &[b"SET", key, value, b"NX"])?;
        }
        Ok(ok)
    }

    /// `SET key value [NX|XX]` with an optional TTL, as one operation under
    /// one lock, so no other thread sees the value without its TTL. The AOF
    /// gets the server's shape: `SET key value PX ms` carries the TTL in the
    /// value's own frame, so a crash before the second frame still leaves an
    /// expiring key, and `PEXPIREAT` then pins the absolute deadline. Only
    /// a SET that happened is logged, so the frame needs no NX/XX.
    pub(crate) fn set_opts(
        &self,
        key: &[u8],
        value: &[u8],
        ttl: Option<Duration>,
        nx: bool,
        xx: bool,
    ) -> KevyResult<bool> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let ok = g.store.set(key, value.to_vec(), ttl, nx, xx);
        if ok {
            match ttl {
                None => commit_write(&mut g, &[b"SET", key, value])?,
                Some(ttl) => {
                    let ms = ttl.as_millis().min(u128::from(u64::MAX)).to_string();
                    commit_write(&mut g, &[b"SET", key, value, b"PX", ms.as_bytes()])?;
                    commit_deadline(&mut g, key)?;
                }
            }
        }
        Ok(ok)
    }

    /// `INCRBYFLOAT key delta` — atomic float increment of a string
    /// value. Returns the post-increment value parsed as f64.
    pub fn incrbyfloat(&self, key: &[u8], delta: f64) -> KevyResult<f64> {
        ensure_writable(self)?;
        let mut g = self.wshard(key);
        let new_bytes = g.store.incr_by_float(key, delta).map_err(store_err)?;
        let delta_str = format!("{delta}");
        commit_write(&mut g, &[b"INCRBYFLOAT", key, delta_str.as_bytes()])?;
        std::str::from_utf8(&new_bytes)
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .ok_or_else(|| KevyError::Protocol("incrbyfloat result not parseable".into()))
    }

    /// `DECR key` — atomic decrement by 1.
    pub fn decr(&self, key: &[u8]) -> KevyResult<i64> {
        self.incr_by(key, -1)
    }

    /// `DECRBY key delta` — atomic decrement by `delta`.
    pub fn decrby(&self, key: &[u8], delta: i64) -> KevyResult<i64> {
        self.incr_by(key, delta.checked_neg().unwrap_or(i64::MIN.saturating_add(1)))
    }

    /// `STRLEN key` — length of the string value at `key`; 0 if
    /// absent. Errors on wrong type.
    pub fn strlen(&self, key: &[u8]) -> KevyResult<usize> {
        self.wshard(key).store.strlen(key).map_err(store_err)
    }

    /// `APPEND key data` — append `data` to the string at `key`.
    /// Creates the key if absent. Returns the new total length.
    pub fn append(&self, key: &[u8], data: &[u8]) -> KevyResult<usize> {
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
    pub fn hsetnx(&self, key: &[u8], field: &[u8], value: &[u8]) -> KevyResult<bool> {
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
