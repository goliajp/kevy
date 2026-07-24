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

use crate::KevyResult;

use crate::store::ensure_writable;
use crate::store::{Store, commit_write};

impl Store {
    /// `COPY src dst [REPLACE]` — copy `src`'s value (and TTL if any)
    /// to `dst`. Returns `true` when the copy happened.
    ///
    /// Semantics:
    /// - `false` if `src` doesn't exist.
    /// - `false` if `dst` exists and `replace = false`.
    /// - Preserves source TTL on the destination via `pexpireat`.
    pub fn copy(&self, src: &[u8], dst: &[u8], replace: bool) -> KevyResult<bool> {
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
        // its own PEXPIREAT. (An earlier regression wrote the dst value
        // to memory only, so it vanished on reopen.)
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
    pub fn unlink(&self, keys: &[&[u8]]) -> KevyResult<usize> {
        self.del(keys)
    }

    /// `TOUCH key [key ...]` — count keys that exist. Side effect:
    /// the existence check refreshes LRU/LFU bookkeeping on the
    /// touched shards, matching Redis semantics.
    pub fn touch(&self, keys: &[&[u8]]) -> KevyResult<usize> {
        self.exists(keys)
    }
}

impl crate::Store {
    /// Order-insensitive prefix checksum for migration
    /// verification: `(row_count, xor_of_row_digests)`. Matches the
    /// server's `PREFIX.DIGEST` bit for bit (same canonicalization).
    pub fn prefix_digest(&self, prefix: &[u8]) -> (u64, u64) {
        let mut pat = prefix.to_vec();
        pat.push(b'*');
        let keys = self.keys(Some(&pat), None);
        let mut xor = 0u64;
        for key in &keys {
            xor ^= self.row_digest_embedded(key);
        }
        (keys.len() as u64, xor)
    }

    /// One row's digest under a SINGLE shard write-lock acquisition,
    /// with the row reads inside the store's bulk-read peek scope
    /// (T6): a cold row hashes from ONE record read, never promotes
    /// and never advances the 2nd-touch gate — a full-prefix digest
    /// must not thrash the hot tier (server twin: `cmd_digest`).
    fn row_digest_embedded(&self, key: &[u8]) -> u64 {
        let mut g = self.wshard(key);
        g.store.peek_scope(|s| {
            let mut h = FNV_OFFSET;
            fnv(&mut h, key);
            let ty = s.type_of(key);
            fnv(&mut h, ty.as_bytes());
            digest_row_body(s, key, ty, &mut h);
            h
        })
    }
}

/// Fold one row's canonicalized value into the FNV state, per type
/// (hash fields and set members sort first; zset folds score bits
/// then member, rank order). Reads the store directly — the caller
/// already holds the shard lock and the peek scope.
fn digest_row_body(s: &mut kevy_store::Store, key: &[u8], ty: &str, h: &mut u64) {
    match ty {
        "string" => {
            if let Ok(Some(v)) = s.get(key) {
                let v = v.to_vec();
                fnv(h, &v);
            }
        }
        "hash" => {
            if let Ok(flat) = s.hgetall(key) {
                let mut pairs: Vec<(&[u8], &[u8])> =
                    flat.chunks(2).map(|c| (c[0].as_slice(), c[1].as_slice())).collect();
                pairs.sort();
                for (f, v) in pairs {
                    fnv(h, f);
                    fnv(h, v);
                }
            }
        }
        "list" => {
            if let Ok(items) = s.lrange(key, 0, -1) {
                for i in items {
                    fnv(h, &i);
                }
            }
        }
        "set" => {
            if let Ok(mut ms) = s.smembers(key) {
                ms.sort();
                for m in ms {
                    fnv(h, &m);
                }
            }
        }
        "zset" => {
            if let Ok(items) = s.zrange(key, 0, -1) {
                for (member, score) in items {
                    fnv(h, &score.to_bits().to_le_bytes());
                    fnv(h, &member);
                }
            }
        }
        _ => {}
    }
}

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(FNV_PRIME);
    }
}
