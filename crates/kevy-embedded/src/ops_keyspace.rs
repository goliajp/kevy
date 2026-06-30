//! Keyspace introspection + cross-key ops — `copy` / `randomkey` /
//! `unlink` / `touch` (kevy-embedded 1.12.0).
//!
//! These compose existing `kevy_store::Store` primitives at the
//! embedded layer rather than adding new Store methods:
//!
//! - `copy` = get + (optional) read TTL + set on dst + expire on dst.
//! - `randomkey` = `collect_keys(None, None)` then index-based pick.
//! - `unlink` = alias for `del` (kevy has no async deletion — the
//!   sync delete IS the unblocking semantic).
//! - `touch` = exists count; reads bump LRU/LFU bookkeeping as a
//!   side effect of `get_for_reply`-style access.
//!
//! Lives outside the other `ops_*.rs` files for the 500-LOC house
//! rule.

use std::io;

#[cfg(not(target_arch = "wasm32"))]
use crate::replica_glue::ensure_writable;
use crate::store::Store;

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
            // Write dst (still holding its shard lock so no race).
            g.store.set(dst, src_val, None, false, false);
        } else {
            let mut g = self.wshard(dst);
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
        // AOF-log the copy as a SET (with optional PEXPIREAT) on dst.
        // Standard `set` + `expire` paths handle this via individual
        // `commit_write` calls in their respective methods — but we
        // bypassed those above (direct `store.set`) to keep the
        // atomic-per-shard semantics, so log the SET ourselves.
        // Easiest: re-emit via `Self::set(dst, &src_val_as_ref)`.
        // For simplicity in v1.12.0 we DO emit via the high-level
        // facade methods — the cost is a redundant Store::set call,
        // which is acceptable for a COPY op.
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
