//! Primary-side replication source — bounded backlog of recent
//! mutations, indexed by monotonic offset.
//!
//! Behaviour at a glance:
//! - [`ReplicationSource::push_mutation`] is called on every applied
//!   write. It assigns the next monotonic offset, encodes the frame
//!   with [`crate::wire::encode_frame`], and appends to the backlog.
//! - The backlog is bounded by a byte budget (`max_bytes`, fed from
//!   `[replication]` `replication_buffer_size` in config). When a new
//!   frame would exceed the budget, the oldest frames are dropped to
//!   make room.
//! - Replicas that disconnect and reconnect within the backlog window
//!   resume via [`ReplicationSource::frames_from`]. Replicas that fall
//!   off the back of the buffer get `Err(FromOffset::TooOld)` and the
//!   caller initiates a full snapshot ship.
//!
//! The source does **not** know about replicas — slot tracking lives
//! in [`crate::slot::SlotTable`]. The source is a passive structure
//! the streaming loop reads; mutation/serialisation lock policy is the
//! wiring layer's concern.

use crate::wire::encode_frame;
use kevy_resp::ArgvView;
#[cfg(test)]
use kevy_resp::Argv;

/// One encoded mutation frame parked in the backlog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Monotonic offset the source assigned at push time.
    pub offset: u64,
    /// Wire-encoded frame bytes (envelope + offset + RESP argv).
    pub bytes: Vec<u8>,
}

/// Reason [`ReplicationSource::frames_from`] cannot serve a replica
/// from the backlog.
#[derive(Debug, PartialEq, Eq)]
pub enum FromOffset {
    /// The replica is asking for an offset we already evicted; the
    /// streaming loop must initiate a snapshot ship.
    TooOld,
    /// The replica's requested offset is greater than the next offset
    /// we would assign — peer is ahead of us (data-dir wipe, epoch
    /// confusion, or bug). The caller should drop the link.
    Future,
}

/// Bounded backlog of recent replicated mutations.
pub struct ReplicationSource {
    next_offset: u64,
    bytes_in_buf: usize,
    max_bytes: usize,
    buf: std::collections::VecDeque<Frame>,
}

impl ReplicationSource {
    /// Create a new source with the given byte budget. `max_bytes` must
    /// be > 0; the source guarantees at most one over-budget frame at
    /// a time (the most recently pushed) so a single huge command does
    /// not silently disappear before its replicas even see it.
    pub fn new(max_bytes: usize) -> Self {
        assert!(max_bytes > 0, "ReplicationSource max_bytes must be > 0");
        Self {
            next_offset: 0,
            bytes_in_buf: 0,
            max_bytes,
            buf: std::collections::VecDeque::new(),
        }
    }

    /// Next offset this source would assign. Equal to one past the
    /// last assigned offset; equals `0` for a fresh source.
    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    /// Lowest offset still in the backlog, or `None` if empty.
    pub fn oldest_offset(&self) -> Option<u64> {
        self.buf.front().map(|f| f.offset)
    }

    /// Highest offset still in the backlog, or `None` if empty.
    pub fn newest_offset(&self) -> Option<u64> {
        self.buf.back().map(|f| f.offset)
    }

    /// Total bytes occupied by frames currently in the backlog.
    pub fn buffered_bytes(&self) -> usize {
        self.bytes_in_buf
    }

    /// Number of frames currently in the backlog.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the backlog has no frames.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Append one applied mutation. Returns the offset assigned to it.
    /// Generic over [`ArgvView`] so the dispatcher's borrowed argv can
    /// flow straight in — no `Argv` materialisation on the write path.
    ///
    /// May evict older frames if the new frame would exceed the byte
    /// budget; the new frame is always retained (even if it is larger
    /// than `max_bytes` on its own — losing the most recent applied
    /// write before any replica has had a chance to ack it would be
    /// a worse failure than briefly running over budget).
    pub fn push_mutation<A: ArgvView + ?Sized>(&mut self, argv: &A) -> u64 {
        let offset = self.next_offset;
        let bytes = encode_frame(offset, argv);
        let frame_len = bytes.len();

        // Evict from the front until either the new frame fits or
        // the buffer is empty.
        while self.bytes_in_buf + frame_len > self.max_bytes && !self.buf.is_empty() {
            let dropped = self.buf.pop_front().expect("non-empty checked");
            self.bytes_in_buf -= dropped.bytes.len();
        }

        self.bytes_in_buf += frame_len;
        self.buf.push_back(Frame { offset, bytes });
        self.next_offset = self
            .next_offset
            .checked_add(1)
            .expect("replication offset wrap — i64::MAX guard tripped");
        offset
    }

    /// Drop every buffered frame whose offset is `< watermark` —
    /// i.e. every replica has consumed past it. Used by the per-
    /// shard tick (T1.22.5) to enforce a retention floor tighter
    /// than the raw byte budget; lets the backlog reclaim space
    /// for live frames once all consumers have advanced.
    ///
    /// No-op when `watermark <= oldest_offset()` (nothing to drop)
    /// or when the buffer is empty. Updates the internal byte
    /// accounting to stay consistent with the live buffer length.
    pub fn drop_up_to(&mut self, watermark: u64) {
        while let Some(front) = self.buf.front() {
            if front.offset >= watermark {
                break;
            }
            let dropped = self.buf.pop_front().expect("front-of-loop");
            self.bytes_in_buf -= dropped.bytes.len();
        }
    }

    /// Borrow the slice of frames with offset ≥ `from`. Suitable for
    /// the streaming loop to write each frame's `bytes` to a replica
    /// socket. Returns:
    /// - `Ok(iter)` — zero or more frames in offset order (empty iter
    ///   means the replica is caught up).
    /// - `Err(FromOffset::TooOld)` — `from` is older than the oldest
    ///   buffered frame; the streaming loop must snapshot-ship.
    /// - `Err(FromOffset::Future)` — `from > next_offset()`; peer is
    ///   ahead of us, drop the link.
    pub fn frames_from(&self, from: u64) -> Result<FramesIter<'_>, FromOffset> {
        if from > self.next_offset {
            return Err(FromOffset::Future);
        }
        // from == next_offset → replica is exactly caught up; empty slice.
        if let Some(oldest) = self.oldest_offset()
            && from < oldest
        {
            return Err(FromOffset::TooOld);
        }
        // Locate the start index. Offsets are monotonic so binary search
        // is correct; the deque slices into two parts so we iterate.
        let start = self.buf.iter().position(|f| f.offset >= from);
        Ok(FramesIter {
            buf: &self.buf,
            cursor: start.unwrap_or(self.buf.len()),
        })
    }
}

/// Iterator over backlog frames returned by [`ReplicationSource::frames_from`].
pub struct FramesIter<'a> {
    buf: &'a std::collections::VecDeque<Frame>,
    cursor: usize,
}

impl<'a> Iterator for FramesIter<'a> {
    type Item = &'a Frame;
    fn next(&mut self) -> Option<&'a Frame> {
        let item = self.buf.get(self.cursor)?;
        self.cursor += 1;
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::decode_frame;

    fn argv(args: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for arg in args {
            a.push(arg);
        }
        a
    }

    #[test]
    fn fresh_source_is_empty() {
        let s = ReplicationSource::new(1024);
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.next_offset(), 0);
        assert_eq!(s.oldest_offset(), None);
        assert_eq!(s.newest_offset(), None);
        assert_eq!(s.buffered_bytes(), 0);
    }

    #[test]
    fn push_assigns_monotonic_offsets() {
        let mut s = ReplicationSource::new(64 * 1024);
        let o0 = s.push_mutation(&argv(&[b"SET", b"a", b"1"]));
        let o1 = s.push_mutation(&argv(&[b"SET", b"b", b"2"]));
        let o2 = s.push_mutation(&argv(&[b"DEL", b"a"]));
        assert_eq!((o0, o1, o2), (0, 1, 2));
        assert_eq!(s.oldest_offset(), Some(0));
        assert_eq!(s.newest_offset(), Some(2));
        assert_eq!(s.next_offset(), 3);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn pushed_frames_decode_back_to_the_pushed_argv() {
        let mut s = ReplicationSource::new(1024);
        let a = argv(&[b"HSET", b"h", b"f", b"v"]);
        let off = s.push_mutation(&a);
        let frame = s.buf.front().expect("one frame");
        assert_eq!(frame.offset, off);
        let (decoded_off, decoded_argv, used) = decode_frame(&frame.bytes).expect("decode");
        assert_eq!(decoded_off, off);
        assert_eq!(decoded_argv, a);
        assert_eq!(used, frame.bytes.len());
    }

    #[test]
    fn eviction_drops_oldest_when_budget_exceeded() {
        // Each frame encodes to ~37 bytes (envelope + offset + 3-arg SET).
        // Budget of 80 bytes holds 2 frames; pushing a 3rd evicts oldest.
        let mut s = ReplicationSource::new(80);
        let _ = s.push_mutation(&argv(&[b"SET", b"a", b"1"]));
        let _ = s.push_mutation(&argv(&[b"SET", b"b", b"2"]));
        assert_eq!(s.oldest_offset(), Some(0));
        let _ = s.push_mutation(&argv(&[b"SET", b"c", b"3"]));
        assert_eq!(s.oldest_offset(), Some(1));
        assert_eq!(s.newest_offset(), Some(2));
        assert!(s.buffered_bytes() <= 80);
        // next_offset keeps climbing even when older frames are evicted.
        assert_eq!(s.next_offset(), 3);
    }

    #[test]
    fn oversized_single_frame_is_retained_against_budget() {
        // Budget of 8 bytes — smaller than any real frame. The most
        // recent push always survives so a freshly-applied write is
        // never lost before any replica can see it.
        let mut s = ReplicationSource::new(8);
        let off = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
        assert_eq!(s.len(), 1);
        assert_eq!(s.oldest_offset(), Some(off));
        assert!(s.buffered_bytes() > 8); // ran over budget; expected.
        // Pushing again still keeps only the newest (older is evicted).
        let off2 = s.push_mutation(&argv(&[b"DEL", b"k"]));
        assert_eq!(s.len(), 1);
        assert_eq!(s.oldest_offset(), Some(off2));
    }

    #[test]
    fn frames_from_at_exact_offset_returns_that_frame_first() {
        let mut s = ReplicationSource::new(1024);
        for i in 0..5 {
            let _ = s.push_mutation(&argv(&[b"SET", b"k", format!("{i}").as_bytes()]));
        }
        let mut it = s.frames_from(2).unwrap();
        let f = it.next().expect("frame");
        assert_eq!(f.offset, 2);
        let remaining: Vec<u64> = it.map(|f| f.offset).collect();
        assert_eq!(remaining, vec![3, 4]);
    }

    #[test]
    fn frames_from_at_next_offset_is_empty_caught_up() {
        let mut s = ReplicationSource::new(1024);
        let _ = s.push_mutation(&argv(&[b"PING"]));
        let _ = s.push_mutation(&argv(&[b"PING"]));
        let it = s.frames_from(s.next_offset()).unwrap();
        assert_eq!(it.count(), 0);
    }

    #[test]
    fn frames_from_too_old_after_eviction() {
        // Tight budget; push enough to evict offset 0.
        let mut s = ReplicationSource::new(80);
        for _ in 0..5 {
            let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
        }
        // Offset 0 was evicted.
        assert!(s.oldest_offset().unwrap() > 0);
        assert!(matches!(s.frames_from(0), Err(FromOffset::TooOld)));
    }

    #[test]
    fn frames_from_future_offset_rejected() {
        let mut s = ReplicationSource::new(1024);
        let _ = s.push_mutation(&argv(&[b"PING"]));
        // next_offset is 1; asking for 2 is a future-offset peer.
        assert!(matches!(s.frames_from(2), Err(FromOffset::Future)));
    }

    #[test]
    fn frames_from_empty_source_at_zero_is_caught_up_not_too_old() {
        // A fresh source has nothing buffered but is at offset 0; a
        // replica asking from 0 is up-to-date (the source has nothing
        // to send yet), not too-old.
        let s = ReplicationSource::new(1024);
        assert_eq!(s.frames_from(0).unwrap().count(), 0);
        // Asking for offset 1 (one past empty next_offset 0) = Future.
        assert!(matches!(s.frames_from(1), Err(FromOffset::Future)));
    }

    #[test]
    fn push_mutation_accepts_argv_borrowed_from_dispatcher_hot_path() {
        // The reactor's local fast path holds the parsed argv as an
        // `ArgvBorrowed` over the connection read buffer (zero-copy);
        // `push_mutation` must accept that view directly, not force
        // a materialised `Argv`. Parse one with the public parser and
        // push it; the decoded round-trip must match a hand-built Argv
        // of the same command.
        let resp = b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        let (borrowed, consumed) = kevy_resp::parse_command_borrowed(resp)
            .expect("parse ok")
            .expect("complete frame");
        assert_eq!(consumed, resp.len());

        let mut s = ReplicationSource::new(1024);
        let off = s.push_mutation(&borrowed);
        assert_eq!(off, 0);

        let frame = s.buf.front().expect("one frame");
        let (decoded_off, decoded_argv, _) =
            crate::wire::decode_frame(&frame.bytes).expect("decode");
        assert_eq!(decoded_off, 0);
        assert_eq!(decoded_argv, argv(&[b"SET", b"foo", b"bar"]));
    }

    #[test]
    fn buffered_bytes_tracks_actual_frame_total() {
        let mut s = ReplicationSource::new(1024);
        let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
        let _ = s.push_mutation(&argv(&[b"DEL", b"k"]));
        let actual: usize = s.buf.iter().map(|f| f.bytes.len()).sum();
        assert_eq!(s.buffered_bytes(), actual);
    }

    #[test]
    fn drop_up_to_evicts_below_watermark() {
        // T1.22.5: drop_up_to(w) evicts every frame with offset < w.
        let mut s = ReplicationSource::new(64 * 1024);
        for i in 0..5 {
            let v = format!("v{i}");
            let _ = s.push_mutation(&argv(&[b"SET", b"k", v.as_bytes()]));
        }
        assert_eq!(s.len(), 5);
        let bytes_before = s.buffered_bytes();
        // Watermark = 3 → drop offsets 0, 1, 2; keep 3, 4.
        s.drop_up_to(3);
        assert_eq!(s.len(), 2);
        assert_eq!(s.oldest_offset(), Some(3));
        assert_eq!(s.newest_offset(), Some(4));
        // bytes accounting must shrink.
        assert!(s.buffered_bytes() < bytes_before);
        // Frames-from at the watermark works without TooOld.
        let kept: Vec<_> = s.frames_from(3).unwrap().collect();
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn drop_up_to_below_oldest_is_noop() {
        let mut s = ReplicationSource::new(64 * 1024);
        let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
        let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
        assert_eq!(s.oldest_offset(), Some(0));
        s.drop_up_to(0); // already-at-or-past-oldest
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn drop_up_to_at_or_past_newest_drops_everything() {
        let mut s = ReplicationSource::new(64 * 1024);
        for _ in 0..3 {
            let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
        }
        s.drop_up_to(99);
        assert!(s.is_empty());
        assert_eq!(s.buffered_bytes(), 0);
    }
}
