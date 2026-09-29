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
    pub generation: u64,
    /// The offset within that history.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeedRead {
    /// Cursor unservable (stale generation or evicted offset): rebuild
    /// from a scan, then resume from the carried tail cursor.
    Resync {
        /// The current generation and the next offset the source will
        /// assign: where to resume.
        tail: FeedPosition,
    },
    /// Cursor is ahead of the stream (`offset > next`) in the CURRENT
    /// generation — caller bug or epoch confusion; reject the read.
    /// (A mismatched generation is `Resync`, never `Future`:
    /// generations carry no order to be "ahead" in.)
    Future,
}

/// One decoded feed entry: the offset plus the frame's wire bytes
/// (envelope + offset + RESP argv — same encoding replicas consume;
/// [`crate::wire::decode_frame`] parses it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FeedFrame<'a> {
    /// Offset the source assigned at push time.
    pub offset: u64,
    /// Wire-encoded frame bytes.
    pub bytes: &'a [u8],
}

/// Generation-aware wrapper: owns the generation number alongside the
/// backlog. The runtime persists `generation` via `kevy-persist`'s
/// feed sidecars; this type only holds the in-memory value.
#[derive(Debug)]
pub struct FeedSource {
    generation: u64,
    source: ReplicationSource,
}

impl FeedSource {
    /// Wrap a backlog at an explicit generation (loaded from the feed
    /// sidecar at boot, or 1 for a fresh data dir).
    pub fn new(generation: u64, source: ReplicationSource) -> Self {
        Self { generation, source }
    }

    /// Current generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Access the wrapped backlog (push side + replica streaming keep
    /// their existing [`ReplicationSource`] API).
    pub fn source(&self) -> &ReplicationSource {
        &self.source
    }

    /// Mutable access for `push_mutation` / `drop_up_to`.
    pub fn source_mut(&mut self) -> &mut ReplicationSource {
        &mut self.source
    }

    /// Break continuity: draw a fresh generation and restart offsets
    /// at 0 (FLUSHALL / restore / promotion / unclean-boot policy).
    /// The backlog empties — frames from the old generation must
    /// never be served under the new one.
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
mod tests {
    use super::*;
    use kevy_resp::Argv;

    fn argv(parts: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for p in parts {
            a.push(p);
        }
        a
    }

    fn feed_with(n: usize) -> FeedSource {
        let mut f = FeedSource::new(1, ReplicationSource::new(1 << 20));
        for i in 0..n {
            f.source_mut().push_mutation(&argv(&[b"SET", format!("k{i}").as_bytes(), b"v"]));
        }
        f
    }

    #[test]
    fn read_serves_in_order_and_caps_at_max() {
        let f = feed_with(5);
        let frames = f.read(FeedPosition::new(1, 0), 3).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].offset, 0);
        assert_eq!(frames[2].offset, 2);
        // caught-up cursor = empty ok
        assert!(f.read(FeedPosition::new(1, 5), 10).unwrap().is_empty());
    }

    #[test]
    fn stale_generation_resyncs_with_tail() {
        let mut f = feed_with(3);
        f.bump_generation();
        let g2 = f.generation();
        f.source_mut().push_mutation(&argv(&[b"SET", b"new", b"v"]));
        match f.read(FeedPosition::new(1, 2), 10) {
            Err(FeedRead::Resync { tail }) => assert_eq!(tail, FeedPosition::new(g2, 1)),
            other => panic!("expected Resync, got {:?}", other.map(|v| v.len())),
        }
        // old-generation frames are gone — new gen serves only its own
        let frames = f.read(FeedPosition::new(g2, 0), 10).unwrap();
        assert_eq!(frames.len(), 1);
    }

    /// The availgate failover wedge, distilled: generations are
    /// identities, not counters. A bump must never land on a
    /// PREDICTABLE next value (old+1 is what a peer node's own bump
    /// would produce for a DIFFERENT history), and any mismatched
    /// cursor — including one "from the future" of a counter's view —
    /// resyncs instead of erroring.
    #[test]
    fn generations_are_random_identities() {
        let mut a = FeedSource::new(1, ReplicationSource::new(1 << 20));
        let mut b = FeedSource::new(1, ReplicationSource::new(1 << 20));
        a.bump_generation();
        b.bump_generation();
        assert_ne!(a.generation(), 0);
        assert_ne!(a.generation(), 1, "old value must not repeat");
        assert_ne!(a.generation(), 2, "a counter's next value is the collision");
        assert_ne!(
            a.generation(),
            b.generation(),
            "two nodes bumping from the same value must diverge"
        );
        // A cursor from a foreign history (any unknown gen) resyncs.
        let g = a.generation();
        match a.read(FeedPosition::new(g.wrapping_add(1), 0), 10) {
            Err(FeedRead::Resync { tail }) => assert_eq!(tail.generation, g),
            other => panic!("expected Resync, got {:?}", other.map(|v| v.len())),
        }
    }

    #[test]
    fn future_cursors_rejected() {
        let f = feed_with(2);
        // Unknown generation (a counter would call gen 3 "the
        // future") → Resync, not Future: identities have no order.
        assert!(matches!(f.read(FeedPosition::new(3, 0), 1), Err(FeedRead::Resync { .. })));
        // Offset ahead within the CURRENT generation → Future.
        assert!(matches!(f.read(FeedPosition::new(1, 99), 1), Err(FeedRead::Future)));
    }

    #[test]
    fn evicted_offset_resyncs() {
        // Tiny budget: pushing enough evicts the front.
        let mut f = FeedSource::new(1, ReplicationSource::new(64));
        for i in 0..50 {
            f.source_mut().push_mutation(&argv(&[b"SET", format!("k{i}").as_bytes(), b"v"]));
        }
        match f.read(FeedPosition::new(1, 0), 10) {
            Err(FeedRead::Resync { tail }) => assert_eq!(tail, FeedPosition::new(1, 50)),
            other => panic!("expected Resync, got {:?}", other.map(|v| v.len())),
        }
    }
}
