//! Cross-key operations: `copy`, `randomkey`, `unlink`, `touch`.
//!
//! These compose existing `kevy_store::Store` primitives at the
//! embedded layer:
//!
//! - `copy` is `get` + (optional) read TTL + `set` on dst + `expire`
//!   on dst.
//! - `randomkey` collects matching keys and picks one by index.
//! - `unlink` is an alias for `del`; kevy has no async deletion, so
//!   sync delete is the unblocking semantic.
//! - `touch` counts existing keys and reads bump LRU/LFU bookkeeping
//!   as a side effect.

use std::io;

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::{Store, commit_write};

#[cfg(target_arch = "wasm32")]
fn ensure_writable(_s: &Store) -> io::Result<()> { Ok(()) }

impl Store {
    /// `COPY src dst [REPLACE]` — copy `src`'s value (and TTL if any)
    /// to `dst`. Returns `true` when the copy happened.
    ///
    /// Semantics:
    /// - `false` if `src` doesn't exist.
    /// - `false` if `dst` exists and `replace = false`.
    /// - Preserves source TTL on the destination via `pexpireat`.
    pub fn copy(&self, src: &[u8], dst: &[u8], replace: bool) -> io::Result<bool> {
        ensure_writable(self)?;
        // Read source under its own shard lock.
        let src_val = match self.get(src)? {
            Some(v) => v,
            None => return Ok(false),
        };
        // Sample the source's TTL (ms since UNIX epoch) BEFORE the
        // write — captures the deadline that should survive the copy.
        let src_ttl_ms = self.ttl_ms(src);
        // Veto if dst exists and replace is false.
        if !replace {
            // Use a fresh wshard on dst so this works cross-shard.
            let mut g = self.wshard(dst);
            if g.store.key_exists(dst) {
                return Ok(false);
            }
            // AOF-log first (SET dst <value>), then write dst — both
            // under dst's shard lock. Log-before-apply avoids cloning
            // the value; an AOF error leaves memory untouched.
            commit_write(&mut g, &[b"SET", dst, &src_val])?;
            g.store.set(dst, src_val, None, false, false);
        } else {
            let mut g = self.wshard(dst);
            commit_write(&mut g, &[b"SET", dst, &src_val])?;
            g.store.set(dst, src_val, None, false, false);
        }
        // Re-attach absolute deadline if the source had one.
        if src_ttl_ms > 0 {
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
                .saturating_add(src_ttl_ms as u64);
            self.pexpireat(dst, unix_ms)?;
        }
        // The dst SET is AOF-logged above under dst's shard lock; the
        // TTL re-attach goes through the `pexpireat` facade which logs
        // its own PEXPIREAT. (v1.15.1: before this, the dst value was
        // written to memory only and vanished on reopen.)
        Ok(true)
    }

    /// `RANDOMKEY` — return a randomly-chosen existing key, or
    /// `None` when the keyspace is empty.
    ///
    /// Implementation: snapshot all keys via `collect_keys`, then
    /// pick a uniform index. For large keyspaces this is O(N); a
    /// future ship can add a `key_at(rank)` Store method for O(1)
    /// random pick.
    pub fn randomkey(&self) -> Option<Vec<u8>> {
        let keys = self.collect_keys(None, None);
        if keys.is_empty() {
            return None;
        }
        // Cheap PRNG via nanosecond clock — embedded in-process so
        // this just needs decent distribution, not crypto strength.
        let idx = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as usize)
            .unwrap_or(0)
            % keys.len();
        Some(keys[idx].clone())
    }

    /// `UNLINK key [key ...]` — alias for [`Self::del`]. In Redis
    /// this is the async (non-blocking) variant; kevy is in-process
    /// so the sync `del` IS the unblocking semantic. Returns count
    /// actually removed.
    pub fn unlink(&self, keys: &[&[u8]]) -> io::Result<usize> {
        self.del(keys)
    }

    /// `TOUCH key [key ...]` — count keys that exist. Side effect:
    /// the existence check refreshes LRU/LFU bookkeeping on the
    /// touched shards, matching Redis semantics.
    pub fn touch(&self, keys: &[&[u8]]) -> io::Result<usize> {
        self.exists(keys)
    }
}
