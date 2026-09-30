//! CDC feed layer over [`crate::source::ReplicationSource`] —
//! the `(generation, offset)` cursor semantics the public FEED.* /
//! `changes_since` surfaces speak.
//!
//! Cursor contract:
//! - `generation` identifies one unbroken offset history. A given
//!   `(gen, offset)` pair refers to the same stream prefix forever.
//!   The value is an OPAQUE random identity, not a counter: two nodes
//!   counting in lockstep (a startup election on one, a failover
//!   promotion on the other) land on the same number for different
//!   histories, and a stale cursor then slips through the fence into
//!   offset aliasing (the availgate failover wedge). Randomness is
//!   what makes cross-node fencing sound; there is no ordering.
//! - Clean shutdown + restart preserves both (continuity); FLUSHALL,
//!   restore-from-snapshot, or an unclean shutdown draws a fresh
//!   `gen` and resets `offset` to 0.
//! - A cursor from any OTHER generation, or one whose offsets were
//!   evicted, answers `FeedRead::Resync` carrying the current tail —
//!   the consumer rebuilds (SCAN) and resumes from there.
//!
//! ```
//! use kevy_replicate::feed::{FeedPosition, FeedRead, FeedSource};
//! use kevy_replicate::source::ReplicationSource;
//!
//! let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
//! let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
//! feed.source_mut().push_mutation(&set);
//! let cursor = FeedPosition::new(1, 0);
//! assert_eq!(feed.read(cursor, 10).map(|f| f.len()), Ok(1));
//!
//! feed.bump_generation(); // FLUSHALL: a new history begins
//! assert!(matches!(feed.read(cursor, 10), Err(FeedRead::Resync { .. })));
//! ```

use crate::source::{FromOffset, ReplicationSource};

/// A point in a feed: an offset within one generation's offset history.
///
/// The pair is the whole identity — an offset means nothing outside its
/// generation, so the two travel together: a feed cursor, the tail a
/// consumer resumes at, the position a replica claims at handshake, the
/// cursor a snapshot records. `FeedPosition::default()` is generation 0,
/// offset 0: the "no continuity claim" a fresh replica presents (no real
/// feed ever runs at generation 0).
///
/// ```
/// use kevy_replicate::feed::FeedPosition;
///
/// let at = FeedPosition::new(7, 42);
/// assert_eq!((at.generation, at.offset), (7, 42));
/// assert_eq!(FeedPosition::default(), FeedPosition::new(0, 0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct FeedPosition {
    /// The offset history this position belongs to: an opaque identity,
    /// see [`fresh_generation`]. `0` = unknown.
    ///
    /// ```
    /// use kevy_replicate::feed::FeedSource;
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let feed = FeedSource::new(9, ReplicationSource::new(1 << 20));
    /// assert_eq!(feed.tail().generation, 9);
    /// ```
    pub generation: u64,
    /// The offset within that history.
    ///
    /// ```
    /// use kevy_replicate::feed::FeedSource;
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// feed.source_mut().push_mutation(&set);
    /// assert_eq!(feed.tail().offset, 1); // the next write lands at offset 1
    /// ```
    pub offset: u64,
}

impl FeedPosition {
    /// The position at `offset` in `generation`.
    ///
    /// ```
    /// let at = kevy_replicate::feed::FeedPosition::new(3, 0);
    /// assert_eq!(at.generation, 3);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(generation: u64, offset: u64) -> Self {
        Self { generation, offset }
    }
}

/// Why a feed read could not be served from the backlog.
///
/// ```
/// use kevy_replicate::feed::{FeedPosition, FeedRead, FeedSource};
/// use kevy_replicate::source::ReplicationSource;
///
/// let feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
/// match feed.read(FeedPosition::new(2, 0), 10) {
///     Err(FeedRead::Resync { tail }) => assert_eq!(tail, FeedPosition::new(1, 0)),
///     other => panic!("unexpected {other:?}"),
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeedRead {
    /// Cursor unservable (stale generation or evicted offset): rebuild
    /// from a scan, then resume from the carried tail cursor.
    ///
    /// ```
    /// use kevy_replicate::feed::{FeedPosition, FeedRead, FeedSource};
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// // a 64-byte backlog keeps only the newest frame
    /// let mut feed = FeedSource::new(1, ReplicationSource::new(64));
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// for _ in 0..3 {
    ///     feed.source_mut().push_mutation(&set);
    /// }
    /// let err = feed.read(FeedPosition::new(1, 0), 10).unwrap_err();
    /// assert_eq!(err, FeedRead::Resync { tail: FeedPosition::new(1, 3) });
    /// ```
    Resync {
        /// The current generation and the next offset the source will
        /// assign: where to resume.
        ///
        /// ```
        /// use kevy_replicate::feed::{FeedPosition, FeedRead, FeedSource};
        /// use kevy_replicate::source::ReplicationSource;
        ///
        /// let feed = FeedSource::new(4, ReplicationSource::new(1 << 20));
        /// let Err(FeedRead::Resync { tail }) = feed.read(FeedPosition::new(3, 0), 10) else { panic!() };
        /// assert!(feed.read(tail, 10)?.is_empty()); // resuming at the tail is served
        /// # Ok::<(), FeedRead>(())
        /// ```
        tail: FeedPosition,
    },
    /// Cursor is ahead of the stream (`offset > next`) in the CURRENT
    /// generation — caller bug or epoch confusion; reject the read.
    /// (A mismatched generation is `Resync`, never `Future`:
    /// generations carry no order to be "ahead" in.)
    ///
    /// ```
    /// use kevy_replicate::feed::{FeedPosition, FeedRead, FeedSource};
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
    /// assert_eq!(feed.read(FeedPosition::new(1, 5), 10), Err(FeedRead::Future));
    /// ```
    Future,
}

/// One decoded feed entry: the offset plus the frame's wire bytes
/// (envelope + offset + RESP argv — same encoding replicas consume;
/// [`crate::wire::decode_frame`] parses it).
///
/// ```
/// use kevy_replicate::feed::{FeedPosition, FeedSource};
/// use kevy_replicate::source::ReplicationSource;
///
/// let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// feed.source_mut().push_mutation(&set);
/// let frames = feed.read(FeedPosition::new(1, 0), 10).expect("offset 0 is buffered");
/// let (decoded, _) = kevy_replicate::wire::decode_frame(frames[0].bytes)?;
/// assert_eq!((frames[0].offset, decoded.argv), (0, set));
/// # Ok::<(), kevy_replicate::wire::WireError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FeedFrame<'a> {
    /// Offset the source assigned at push time.
    ///
    /// ```
    /// use kevy_replicate::feed::{FeedPosition, FeedSource};
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// for _ in 0..3 {
    ///     feed.source_mut().push_mutation(&set);
    /// }
    /// let frames = feed.read(FeedPosition::new(1, 1), 10)?;
    /// assert_eq!(frames.iter().map(|f| f.offset).collect::<Vec<_>>(), [1, 2]);
    /// # Ok::<(), kevy_replicate::feed::FeedRead>(())
    /// ```
    pub offset: u64,
    /// Wire-encoded frame bytes.
    ///
    /// ```
    /// use kevy_replicate::feed::{FeedPosition, FeedSource};
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// feed.source_mut().push_mutation(&set);
    /// let frames = feed.read(FeedPosition::new(1, 0), 1)?;
    /// assert_eq!(frames[0].bytes, kevy_replicate::wire::encode_frame(0, &set));
    /// # Ok::<(), kevy_replicate::feed::FeedRead>(())
    /// ```
    pub bytes: &'a [u8],
}

/// Generation-aware wrapper: owns the generation number alongside the
/// backlog. The runtime persists `generation` via `kevy-persist`'s
/// feed sidecars; this type only holds the in-memory value.
///
/// ```
/// use kevy_replicate::feed::{FeedPosition, FeedSource};
/// use kevy_replicate::source::ReplicationSource;
///
/// let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// feed.source_mut().push_mutation(&set);
/// let mut cursor = FeedPosition::new(1, 0);
/// for frame in feed.read(cursor, 100)? {
///     cursor.offset = frame.offset + 1; // a consumer advances past what it applied
/// }
/// assert_eq!(cursor, feed.tail());
/// # Ok::<(), kevy_replicate::feed::FeedRead>(())
/// ```
#[derive(Debug)]
pub struct FeedSource {
    generation: u64,
    source: ReplicationSource,
}

impl FeedSource {
    /// Wrap a backlog at an explicit generation (loaded from the feed
    /// sidecar at boot, or 1 for a fresh data dir).
    ///
    /// ```
    /// use kevy_replicate::feed::{FeedPosition, FeedSource};
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
    /// assert_eq!(feed.tail(), FeedPosition::new(1, 0));
    /// ```
    pub fn new(generation: u64, source: ReplicationSource) -> Self {
        Self { generation, source }
    }

    /// Current generation.
    ///
    /// ```
    /// use kevy_replicate::feed::FeedSource;
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
    /// assert_eq!(feed.generation(), 1);
    /// feed.bump_generation();
    /// assert_ne!(feed.generation(), 1);
    /// ```
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Access the wrapped backlog (push side + replica streaming keep
    /// their existing [`ReplicationSource`] API).
    ///
    /// ```
    /// use kevy_replicate::feed::FeedSource;
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let feed = FeedSource::new(1, ReplicationSource::new(4096));
    /// assert_eq!(feed.source().max_bytes(), 4096);
    /// assert!(feed.source().is_empty());
    /// ```
    pub fn source(&self) -> &ReplicationSource {
        &self.source
    }

    /// Mutable access for `push_mutation` / `drop_up_to`.
    ///
    /// ```
    /// use kevy_replicate::feed::FeedSource;
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// assert_eq!(feed.source_mut().push_mutation(&set), 0);
    /// feed.source_mut().drop_up_to(1); // every consumer has passed offset 0
    /// assert!(feed.source().is_empty());
    /// ```
    pub fn source_mut(&mut self) -> &mut ReplicationSource {
        &mut self.source
    }

    /// Break continuity: draw a fresh generation and restart offsets
    /// at 0 (FLUSHALL / restore / promotion / unclean-boot policy).
    /// The backlog empties — frames from the old generation must
    /// never be served under the new one.
    ///
    /// ```
    /// use kevy_replicate::feed::{FeedPosition, FeedSource};
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// feed.source_mut().push_mutation(&set);
    /// feed.bump_generation();
    /// assert_eq!(feed.tail(), FeedPosition::new(feed.generation(), 0));
    /// assert!(feed.source().is_empty());
    /// ```
    pub fn bump_generation(&mut self) {
        self.generation = fresh_generation(self.generation);
        let budget = self.source.max_bytes();
        self.source = ReplicationSource::new(budget);
    }

    /// The tail cursor: the current generation and the next offset —
    /// where a consumer resuming after a rebuild starts.
    ///
    /// ```
    /// use kevy_replicate::feed::{FeedPosition, FeedSource};
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let feed = FeedSource::new(5, ReplicationSource::new(1 << 20));
    /// assert_eq!(feed.tail(), FeedPosition::new(5, 0));
    /// ```
    #[inline]
    pub fn tail(&self) -> FeedPosition {
        FeedPosition::new(self.generation, self.source.next_offset())
    }

    /// Serve up to `max` frames at cursor `at`.
    ///
    /// - Any OTHER generation → `Resync` (the cursor belongs to a
    ///   different offset history — uniqueness of `(gen, offset)`
    ///   forbids serving it; generations are identities, not ordered).
    /// - Offset ahead of this generation's stream → `Future`.
    /// - Evicted offset → `Resync` with the current tail.
    ///
    /// ```
    /// use kevy_replicate::feed::{FeedPosition, FeedSource};
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let mut feed = FeedSource::new(1, ReplicationSource::new(1 << 20));
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// for _ in 0..5 {
    ///     feed.source_mut().push_mutation(&set);
    /// }
    /// let page = feed.read(FeedPosition::new(1, 0), 2)?; // at most 2 frames
    /// assert_eq!(page.iter().map(|f| f.offset).collect::<Vec<_>>(), [0, 1]);
    /// # Ok::<(), kevy_replicate::feed::FeedRead>(())
    /// ```
    pub fn read(&self, at: FeedPosition, max: usize) -> Result<Vec<FeedFrame<'_>>, FeedRead> {
        if at.generation != self.generation {
            return Err(FeedRead::Resync { tail: self.tail() });
        }
        match self.source.frames_from(at.offset) {
            Ok(iter) => Ok(iter
                .take(max)
                .map(|f| FeedFrame { offset: f.offset, bytes: &f.bytes })
                .collect()),
            Err(FromOffset::TooOld) => Err(FeedRead::Resync { tail: self.tail() }),
            Err(FromOffset::Future) => Err(FeedRead::Future),
        }
    }
}

/// Draw a fresh generation: a random nonzero u64 distinct from `old`.
/// A generation is a HISTORY IDENTITY. Counters are unsound here: two
/// nodes bumping in lockstep (startup election vs failover promotion)
/// collide on the same number for different histories, and a replica's
/// stale cursor then passes the generation fence into offset aliasing
/// — served as "caught up" at a foreign offset, it wedges forever
/// (the availgate failover-convergence flake). `RandomState`
/// carries the process's OS-seeded entropy; identity, not crypto.
///
/// Values fit in 53 bits, so they survive a round trip through a
/// JavaScript number.
///
/// ```
/// let g = kevy_replicate::feed::fresh_generation(1);
/// assert!(g != 0 && g != 1 && g < 1 << 53);
/// ```
#[must_use]
pub fn fresh_generation(old: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    loop {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(old);
        // Mask to 53 bits: generations ride RESP integers (REPL.TOKEN
        // / REPL.WAIT / FEED.TAIL), and client bindings surface them
        // as JS Numbers, whose integer precision ends at 2^53 — a
        // wider value round-trips corrupted and self-resyncs forever.
        // A 2^53 identity space still makes collisions negligible.
        let g = h.finish() & ((1u64 << 53) - 1);
        if g != 0 && g != old {
            return g;
        }
    }
}

#[cfg(test)]
#[path = "feed_tests.rs"]
mod tests;
