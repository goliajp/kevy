//! CDC consumer surface — embedded half.
//! One stream per store: the embedded write path already
//! serializes every shard's mutations through `commit_write`, so the
//! feed is a single `(generation, offset)` stream and
//! [`Store::feed_shards`] reports 1 (server-parity consumer loops work
//! unchanged; they just see one shard).
//!
//! Persistence: with a `data_dir`, the generation contract rides the
//! same `feed-0.gen` / `feed-0.meta` sidecars the server uses (clean
//! close keeps the cursor, crash or FLUSHALL bumps). Without
//! persistence the store's data dies with the process anyway — each
//! open starts a fresh generation-1 stream, which is exactly what the
//! (empty) restored state implies.

use crate::KevyResult;
use std::sync::{Arc, Mutex};

use kevy_replicate::feed::{FeedPosition, FeedRead, FeedSource};

use crate::store::Store;

/// One mutation delivered by [`Store::changes_since`].
///
/// ```
/// use kevy_embedded::{Config, Store};
///
/// let store = Store::open(Config::default().with_feed(0))?;
/// let from = store.changes_tail()?;
/// store.set(b"k", b"v")?;
/// let batch = store.changes_since(from, 10, &[])?;
/// let change = &batch.changes[0];
/// assert_eq!(change.offset, from.offset);
/// assert_eq!(change.argv[0], b"SET");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Change {
    /// Stream offset (monotonic within a generation).
    pub offset: u64,
    /// The applied effect's argv (same frames the AOF / a replica sees).
    pub argv: Vec<Vec<u8>>,
}

impl Change {
    /// A change at `offset` carrying `argv` — what a client decoding a
    /// server's `FEED.READ` reply builds, so both backends hand back
    /// the same type.
    ///
    /// ```
    /// let c = kevy_embedded::Change::new(7, vec![b"DEL".to_vec(), b"k".to_vec()]);
    /// assert_eq!((c.offset, c.argv.len()), (7, 2));
    /// ```
    #[inline]
    pub fn new(offset: u64, argv: Vec<Vec<u8>>) -> Self {
        Self { offset, argv }
    }
}

/// A batch of changes plus the cursor to resume from.
///
/// ```
/// use kevy_embedded::{Config, Store};
///
/// let store = Store::open(Config::default().with_feed(0))?;
/// let from = store.changes_tail()?;
/// store.set(b"k", b"v")?;
/// let batch = store.changes_since(from, 10, &[])?;
/// assert_eq!(batch.changes.len(), 1);
/// assert_eq!(batch.next.offset, from.offset + 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ChangeBatch {
    /// Delivered changes, offset order.
    pub changes: Vec<Change>,
    /// The cursor to pass to the next `changes_since`.
    pub next: FeedPosition,
}

impl ChangeBatch {
    /// A batch of `changes` resuming at `next` — the decoded form of a
    /// server's `FEED.READ` reply.
    ///
    /// ```
    /// use kevy_embedded::{ChangeBatch, FeedPosition};
    /// let caught_up = ChangeBatch::new(Vec::new(), FeedPosition::new(1, 42));
    /// assert!(caught_up.changes.is_empty());
    /// assert_eq!(caught_up.next.offset, 42);
    /// ```
    #[inline]
    pub fn new(changes: Vec<Change>, next: FeedPosition) -> Self {
        Self { changes, next }
    }
}

/// Why a feed read could not be served.
///
/// ```
/// use kevy_embedded::{Config, FeedError, Store};
///
/// let store = Store::open(Config::default())?;
/// assert_eq!(store.changes_tail(), Err(FeedError::Disabled));
/// assert_eq!(FeedError::Disabled.to_string(), "the store was opened without a change feed");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FeedError {
    /// Cursor unservable (stale generation / evicted offsets): rebuild
    /// from a scan, then resume from `tail`.
    Resync {
        /// The current generation and the offset to resume from.
        tail: FeedPosition,
    },
    /// Cursor is ahead of the stream — caller bug.
    Future,
    /// The store was opened without `Config::with_feed`.
    Disabled,
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resync { tail } => write!(
                f,
                "feed cursor unservable, resync from generation {} offset {}",
                tail.generation, tail.offset
            ),
            Self::Future => f.write_str("feed cursor ahead of stream"),
            Self::Disabled => f.write_str("the store was opened without a change feed"),
        }
    }
}

impl std::error::Error for FeedError {}

impl FeedError {
    /// The error line FEED.TAIL / FEED.READ answer with on the embedded
    /// wire surfaces, unchanged since those verbs shipped.
    pub(crate) fn wire_text(&self) -> String {
        match self {
            Self::Resync { tail } => format!(
                "ERR feed: Resync {{ generation: {}, tail: {} }}",
                tail.generation, tail.offset
            ),
            Self::Future => "ERR feed: Future".to_owned(),
            Self::Disabled => "ERR feed: Disabled".to_owned(),
        }
    }
}

/// Multi-key / keyless verbs the fail-open prefix filter never drops
/// (their key layout isn't argv[1], or they touch everything).
const FILTER_DENYLIST: &[&[u8]] = &[
    b"DEL",
    b"UNLINK",
    b"MSET",
    b"COPY",
    b"RENAME",
    b"FLUSHALL",
    b"BITOP",
    b"SINTERSTORE",
    b"SUNIONSTORE",
    b"SDIFFSTORE",
    b"ZINTERSTORE",
    b"ZUNIONSTORE",
    b"ZDIFFSTORE",
];

fn matches_prefixes(argv: &[Vec<u8>], prefixes: &[&[u8]]) -> bool {
    if prefixes.is_empty() {
        return true;
    }
    let Some(verb) = argv.first() else { return true };
    if FILTER_DENYLIST.iter().any(|d| verb.eq_ignore_ascii_case(d)) {
        return true; // fail-open: over-delivery is free, drops are not
    }
    match argv.get(1) {
        Some(key) => prefixes.iter().any(|p| key.starts_with(p)),
        None => true,
    }
}

/// Per-prefix keyspace stats from [`Store::info_prefix`].
///
/// ```
/// let store = kevy_embedded::Store::open(kevy_embedded::Config::default())?;
/// store.set(b"user:1", b"a")?;
/// store.set(b"order:1", b"b")?;
/// let info = store.info_prefix(b"user:");
/// assert_eq!((info.keys, info.expires), (1, 0));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct PrefixInfo {
    /// Live keys under the prefix.
    pub keys: u64,
    /// How many of them carry a TTL.
    pub expires: u64,
}

impl Store {
    /// `info_prefix`: count live keys (and TTL'd keys) under a
    /// byte prefix, across all shards. O(keyspace) — an ops/stats
    /// call, not a hot-path primitive.
    pub fn info_prefix(&self, prefix: &[u8]) -> PrefixInfo {
        let mut keys = 0u64;
        let mut expires = 0u64;
        for shard in self.shards.iter() {
            let g = crate::store::lock_read(shard);
            let (k, e) = g.store.prefix_stats(prefix);
            keys += k;
            expires += e;
        }
        PrefixInfo { keys, expires }
    }

    /// Number of independent change streams this store exposes (the
    /// embedded write path serializes all shards: always 1).
    pub fn feed_shards(&self) -> usize {
        1
    }

    /// The current cursor: the generation and the next offset — where a
    /// consumer starting fresh (or resuming after a rebuild) begins.
    ///
    /// ```
    /// use kevy_embedded::{Config, Store};
    ///
    /// let store = Store::open(Config::default().with_feed(0))?;
    /// assert_eq!(store.changes_tail()?.offset, 0);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn changes_tail(&self) -> Result<FeedPosition, FeedError> {
        let feed = self.feed_handle().ok_or(FeedError::Disabled)?;
        let g = feed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(g.tail())
    }

    /// Deliver up to `limit` changes at cursor `from`, optionally
    /// prefix-filtered (fail-open on multi-key verbs; the filter never
    /// affects the returned cursor). At-least-once: after a `Resync`
    /// rebuild, frames already applied may be seen again.
    ///
    /// ```
    /// use kevy_embedded::{Config, Store};
    ///
    /// let store = Store::open(Config::default().with_feed(0))?;
    /// let from = store.changes_tail()?;
    /// store.set(b"user:1", b"a")?;
    /// store.set(b"order:1", b"b")?;
    /// let batch = store.changes_since(from, 10, &[b"user:"])?;
    /// assert_eq!(batch.changes.len(), 1);
    /// assert_eq!(batch.next.offset, from.offset + 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn changes_since(
        &self,
        from: FeedPosition,
        limit: usize,
        prefixes: &[&[u8]],
    ) -> Result<ChangeBatch, FeedError> {
        let feed = self.feed_handle().ok_or(FeedError::Disabled)?;
        let g = feed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let frames = match g.read(from, limit.clamp(1, 65536)) {
            Ok(v) => v,
            Err(FeedRead::Resync { tail }) => return Err(FeedError::Resync { tail }),
            Err(FeedRead::Future) => return Err(FeedError::Future),
        };
        let next_off = frames.last().map_or(from.offset, |f| f.offset + 1);
        let mut changes = Vec::with_capacity(frames.len());
        for f in &frames {
            let Ok((kevy_replicate::replica::DecodedFrame { offset: foff, argv, .. }, _)) =
                kevy_replicate::wire::decode_frame(f.bytes)
            else {
                continue;
            };
            let owned: Vec<Vec<u8>> = (0..argv.len()).map(|i| argv[i].to_vec()).collect();
            if !matches_prefixes(&owned, prefixes) {
                continue;
            }
            changes.push(Change { offset: foff, argv: owned });
        }
        Ok(ChangeBatch { changes, next: FeedPosition::new(g.generation(), next_off) })
    }

    /// Feed hooks used by `commit_write` / `flushall` / close —
    /// `None` unless the store was opened with feed enabled.
    pub(crate) fn feed_handle(&self) -> Option<&Arc<Mutex<FeedSource>>> {
        self.feed.as_ref()
    }

    /// Break stream continuity on FLUSHALL: bump + persist the
    /// generation high-water (mirrors the server's exec_op Flush arm).
    pub(crate) fn feed_bump_on_flush(&self) {
        let Some(feed) = self.feed_handle() else { return };
        let mut g = feed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.bump_generation();
        if let Some(dir) = &self.config.data_dir
            && let Err(e) = kevy_persist::feed_meta::write_feed_gen(dir, 0, g.generation())
        {
            eprintln!("kevy-embedded: feed gen write failed: {e}");
        }
    }

    /// Clean-close half of the continuity contract (called from the
    /// DropGuard after the AOF flush).
    pub(crate) fn feed_write_close_marker(
        shards_feed: &Arc<Mutex<FeedSource>>,
        dir: &std::path::Path,
    ) {
        let g = shards_feed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(e) = kevy_persist::feed_meta::write_feed_meta(dir, 0, g.tail()) {
            eprintln!("kevy-embedded: feed marker write failed: {e}");
        }
    }

    /// Push one applied effect into the feed (called from
    /// `commit_write` alongside the AOF append).
    pub(crate) fn feed_push(feed: &Arc<Mutex<FeedSource>>, parts: &[&[u8]]) {
        let mut g = feed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut argv = kevy_resp::Argv::default();
        for p in parts {
            argv.push(p);
        }
        let _ = g.source_mut().push_mutation(&argv);
    }

    /// Feed boot half for `Store::open` — resolve the cursor via the
    /// sidecar decision table when persistent, else a fresh gen-1.
    pub(crate) fn feed_open(
        config: &crate::config::Config,
    ) -> KevyResult<Option<Arc<Mutex<FeedSource>>>> {
        if !config.feed_enabled {
            return Ok(None);
        }
        let budget = usize::try_from(config.feed_buffer_size).unwrap_or(usize::MAX);
        let at = match &config.data_dir {
            Some(dir) => kevy_persist::feed_meta::boot_position(dir, 0)?,
            None => FeedPosition::new(1, 0),
        };
        let mut src = kevy_replicate::source::ReplicationSource::new(budget);
        src.set_next_offset(at.offset);
        Ok(Some(Arc::new(Mutex::new(FeedSource::new(at.generation, src)))))
    }
}
