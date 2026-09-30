//! Cross-key operations: `copy`, `randomkey`, `unlink`, `touch`.
//!
//! These compose existing `kevy_store::Store` primitives at the
//! embedded layer:
//!
//! - `copy` clones the source's value and TTL and places them at dst.
//! - `randomkey` collects matching keys and picks one by index.
//! - `unlink` is an alias for `del`; kevy has no async deletion, so
//!   sync delete is the unblocking semantic.
//! - `touch` counts existing keys and reads bump LRU/LFU bookkeeping
//!   as a side effect.

use crate::KevyResult;

use crate::CopyMode;
use crate::store::ensure_writable;
use crate::store::{Inner, Store, commit_write};

impl Store {
    /// `COPY src dst [REPLACE]` — copy `src`'s value, of any type, and
    /// its remaining TTL to `dst`. Returns `true` when the copy happened.
    ///
    /// Semantics:
    /// - `false` if `src` doesn't exist.
    /// - `false` if `dst` exists and `mode` is [`CopyMode::IfAbsent`].
    /// - A source with a TTL gives the destination the same deadline.
    ///
    /// ```
    /// use kevy_embedded::{Config, CopyMode, Store};
    ///
    /// let s = Store::open(Config::default())?;
    /// s.set(b"a", b"1")?;
    /// assert!(s.copy(b"a", b"b", CopyMode::IfAbsent)?);
    /// assert_eq!(s.get(b"b")?.as_deref(), Some(&b"1"[..]));
    /// s.hset(b"h", &[(b"f", b"v")])?;
    /// assert!(s.copy(b"h", b"b", CopyMode::Replace)?);
    /// assert_eq!(s.hget(b"b", b"f")?.as_deref(), Some(&b"v"[..]));
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub fn copy(&self, src: &[u8], dst: &[u8], mode: CopyMode) -> KevyResult<bool> {
        ensure_writable(self)?;
        // the source's lock is released before the destination's is
        // taken: the two keys may live on one shard
        let cloned = self.wshard(src).store.clone_with_ttl(src);
        let Some((value, ttl_ms)) = cloned else {
            return Ok(false);
        };
        let mut g = self.wshard(dst);
        let replaced = g.store.key_exists(dst);
        if replaced && !matches!(mode, CopyMode::Replace) {
            return Ok(false);
        }
        let frames = placed_frames(&g, dst, &value, ttl_ms);
        g.store.put_with_ttl(dst.to_vec(), value, ttl_ms);
        commit_copy(&mut g, [b"COPY", src, dst], frames, replaced)?;
        Ok(true)
    }

    /// `RANDOMKEY` — return a randomly-chosen existing key, or
    /// `None` when the keyspace is empty.
    ///
    /// A shard is drawn in proportion to how many keys it holds, and that
    /// shard picks from a random point in its table, so the cost does not
    /// grow with the keyspace.
    ///
    /// ```
    /// # use kevy_embedded::{Config, Store};
    /// let s = Store::open(Config::default())?;
    /// assert_eq!(s.randomkey(), None);
    /// s.set(b"only", b"1")?;
    /// assert_eq!(s.randomkey(), Some(b"only".to_vec()));
    /// # Ok::<(), kevy_embedded::KevyError>(())
    /// ```
    pub fn randomkey(&self) -> Option<Vec<u8>> {
        let sizes: Vec<usize> =
            self.shards.iter().map(|sh| crate::store_glue::lock_read(sh).store.dbsize()).collect();
        let total: usize = sizes.iter().sum();
        if total == 0 {
            return None;
        }
        let mut draw = {
            let mut g = crate::store_glue::lock_write(&self.shards[0]);
            g.store.rand_draw() as usize % total
        };
        for (i, n) in sizes.iter().enumerate() {
            if draw < *n {
                if let Some(k) = crate::store_glue::lock_write(&self.shards[i]).store.random_key() {
                    return Some(k);
                }
                break;
            }
            draw -= n;
        }
        // the shard drawn emptied since it was counted: any key will do
        self.shards.iter().find_map(|sh| crate::store_glue::lock_write(sh).store.random_key())
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
    /// — a cold row hashes from ONE record read, never promotes
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

/// The commands that rebuild a copied value at `dst`, when this shard
/// records its writes anywhere
#[cfg(feature = "persist")]
fn placed_frames(
    g: &Inner,
    dst: &[u8],
    value: &kevy_store::Value,
    ttl_ms: Option<u64>,
) -> Option<Vec<u8>> {
    crate::store_glue::records_writes(g)
        .then(|| kevy_persist::value_as_v1_frames(dst, value, ttl_ms))
}

#[cfg(not(feature = "persist"))]
fn placed_frames(_: &Inner, _: &[u8], _: &kevy_store::Value, _: Option<u64>) -> Option<Vec<u8>> {
    None
}

/// Record a copy as the commands that rebuild its value, after a DEL
/// when it replaced a key, so a replay does not merge into what was
/// there; with nothing to record into, the argv runs the commit's other
/// steps
fn commit_copy(
    g: &mut Inner,
    argv: [&[u8]; 3],
    frames: Option<Vec<u8>>,
    replaced: bool,
) -> KevyResult<()> {
    let Some(frames) = frames else {
        return commit_write(g, &argv);
    };
    if replaced {
        commit_write(g, &[b"DEL", argv[2]])?;
    }
    let (mut pos, mut cmd) = (0, kevy_resp::Argv::default());
    while pos < frames.len() {
        cmd.clear();
        pos += kevy_resp::parse_command_into(&frames[pos..], &mut cmd)
            .ok()
            .flatten()
            .expect("the value serializer writes whole commands");
        let parts: Vec<&[u8]> = cmd.iter().collect();
        commit_write(g, &parts)?;
    }
    Ok(())
}
