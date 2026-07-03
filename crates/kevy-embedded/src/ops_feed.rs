//! v2.3 CDC consumer surface — embedded half (RFC 2026-07-04, LOCKED,
//! D7). One stream per store: the embedded write path already
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

use std::io;
use std::sync::{Arc, Mutex};

use kevy_replicate::feed::{FeedRead, FeedSource};

use crate::store::Store;

/// One mutation delivered by [`Store::changes_since`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Stream offset (monotonic within a generation).
    pub offset: u64,
    /// The applied effect's argv (same frames the AOF / a replica sees).
    pub argv: Vec<Vec<u8>>,
}

/// A batch of changes plus the cursor to resume from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeBatch {
    /// Delivered changes, offset order.
    pub changes: Vec<Change>,
    /// `(generation, offset)` to pass to the next `changes_since`.
    pub next: (u64, u64),
}

/// Why a feed read could not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedError {
    /// Cursor unservable (stale generation / evicted offsets): rebuild
    /// from a scan, then resume from `tail`.
    Resync {
        /// Current generation.
        generation: u64,
        /// Resume offset.
        tail: u64,
    },
    /// Cursor is ahead of the stream — caller bug.
    Future,
    /// The store was opened without `Config::with_feed`.
    Disabled,
}

/// Multi-key / keyless verbs the fail-open prefix filter never drops
/// (their key layout isn't argv[1], or they touch everything).
const FILTER_DENYLIST: &[&[u8]] = &[
    b"DEL", b"UNLINK", b"MSET", b"COPY", b"RENAME", b"FLUSHALL", b"BITOP",
    b"SINTERSTORE", b"SUNIONSTORE", b"SDIFFSTORE",
    b"ZINTERSTORE", b"ZUNIONSTORE", b"ZDIFFSTORE",
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

impl Store {
    /// Number of independent change streams this store exposes (the
    /// embedded write path serializes all shards: always 1).
    pub fn feed_shards(&self) -> usize {
        1
    }

    /// The current `(generation, next_offset)` cursor — where a
    /// consumer starting fresh (or resuming after a rebuild) begins.
    pub fn changes_tail(&self) -> Result<(u64, u64), FeedError> {
        let feed = self.feed_handle().ok_or(FeedError::Disabled)?;
        let g = feed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(g.tail())
    }

    /// Deliver up to `limit` changes at cursor `(generation, offset)`,
    /// optionally prefix-filtered (fail-open on multi-key verbs; the
    /// filter never affects the returned cursor). At-least-once: after
    /// a `Resync` rebuild, frames already applied may be seen again.
    pub fn changes_since(
        &self,
        generation: u64,
        offset: u64,
        limit: usize,
        prefixes: &[&[u8]],
    ) -> Result<ChangeBatch, FeedError> {
        let feed = self.feed_handle().ok_or(FeedError::Disabled)?;
        let g = feed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let frames = match g.read(generation, offset, limit.clamp(1, 65536)) {
            Ok(v) => v,
            Err(FeedRead::Resync { generation, tail }) => {
                return Err(FeedError::Resync { generation, tail });
            }
            Err(FeedRead::Future) => return Err(FeedError::Future),
        };
        let next_off = frames.last().map_or(offset, |f| f.offset + 1);
        let mut changes = Vec::with_capacity(frames.len());
        for f in &frames {
            let Ok((foff, argv, _)) = kevy_replicate::wire::decode_frame(f.bytes) else {
                continue;
            };
            let owned: Vec<Vec<u8>> = (0..argv.len()).map(|i| argv[i].to_vec()).collect();
            if !matches_prefixes(&owned, prefixes) {
                continue;
            }
            changes.push(Change { offset: foff, argv: owned });
        }
        Ok(ChangeBatch { changes, next: (g.generation(), next_off) })
    }

    /// v2.3 feed hooks used by `commit_write` / `flushall` / close —
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
    pub(crate) fn feed_write_close_marker(shards_feed: &Arc<Mutex<FeedSource>>, dir: &std::path::Path) {
        let g = shards_feed.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (generation, next) = g.tail();
        if let Err(e) = kevy_persist::feed_meta::write_feed_meta(dir, 0, generation, next) {
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
    ) -> io::Result<Option<Arc<Mutex<FeedSource>>>> {
        if !config.feed_enabled {
            return Ok(None);
        }
        let budget = usize::try_from(config.feed_buffer_size).unwrap_or(usize::MAX);
        let (generation, next_offset) = match &config.data_dir {
            Some(dir) => {
                let b = kevy_persist::feed_meta::load_feed_boot(dir, 0)?;
                (b.generation, b.next_offset)
            }
            None => (1, 0),
        };
        let mut src = kevy_replicate::source::ReplicationSource::new(budget);
        src.set_next_offset(next_offset);
        Ok(Some(Arc::new(Mutex::new(FeedSource::new(generation, src)))))
    }
}
