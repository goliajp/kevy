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
//!
//! ```
//! use kevy_replicate::source::{FromOffset, ReplicationSource};
//!
//! let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
//! let frame_len = kevy_replicate::wire::encode_frame(0, &set).len();
//! let mut backlog = ReplicationSource::new(2 * frame_len); // room for two frames
//! for _ in 0..3 {
//!     backlog.push_mutation(&set);
//! }
//! // a replica that acked offset 2 resumes from the backlog ...
//! assert_eq!(backlog.frames_from(2).map(Iterator::count), Ok(1));
//! // ... one still at offset 0 fell off the back and needs a snapshot
//! assert!(matches!(backlog.frames_from(0), Err(FromOffset::TooOld)));
//! ```

use crate::wire::encode_frame;
#[cfg(test)]
use kevy_resp::Argv;
use kevy_resp::ArgvView;

/// One encoded mutation frame parked in the backlog.
///
/// ```
/// use kevy_replicate::source::ReplicationSource;
///
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// let mut backlog = ReplicationSource::new(1 << 20);
/// backlog.push_mutation(&set);
/// let frame = backlog.frames_from(0).ok().and_then(|mut f| f.next()).expect("offset 0 is buffered");
/// let (decoded, used) = kevy_replicate::wire::decode_frame(&frame.bytes)?;
/// assert_eq!((decoded.offset, decoded.argv, used), (frame.offset, set, frame.bytes.len()));
/// # Ok::<(), kevy_replicate::wire::WireError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Frame {
    /// Monotonic offset the source assigned at push time.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// let assigned = backlog.push_mutation(&set);
    /// assert_eq!(backlog.frames_from(0)?.next().map(|f| f.offset), Some(assigned));
    /// # Ok::<(), kevy_replicate::source::FromOffset>(())
    /// ```
    pub offset: u64,
    /// Wire-encoded frame bytes (envelope + offset + RESP argv).
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// backlog.push_mutation(&set);
    /// let frame = backlog.frames_from(0)?.next().expect("one frame");
    /// assert_eq!(frame.bytes, b"*2\r\n:0\r\n*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
    /// # Ok::<(), kevy_replicate::source::FromOffset>(())
    /// ```
    pub bytes: Vec<u8>,
}

/// Reason [`ReplicationSource::frames_from`] cannot serve a replica
/// from the backlog.
///
/// ```
/// use kevy_replicate::source::{FromOffset, ReplicationSource};
///
/// let backlog = ReplicationSource::new(1 << 20);
/// let action = match backlog.frames_from(7) {
///     Err(FromOffset::TooOld) => "ship a snapshot",
///     Err(FromOffset::Future) => "drop the link", // the replica is ahead of us
///     Ok(_frames) => "stream the frames",
/// };
/// assert_eq!(action, "drop the link");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FromOffset {
    /// The replica is asking for an offset we already evicted; the
    /// streaming loop must initiate a snapshot ship.
    ///
    /// ```
    /// use kevy_replicate::source::{FromOffset, ReplicationSource};
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// backlog.push_mutation(&set);
    /// backlog.push_mutation(&set);
    /// backlog.drop_up_to(1); // offset 0 is gone
    /// assert!(matches!(backlog.frames_from(0), Err(FromOffset::TooOld)));
    /// ```
    TooOld,
    /// The replica's requested offset is greater than the next offset
    /// we would assign — peer is ahead of us (data-dir wipe, epoch
    /// confusion, or bug). The caller should drop the link.
    ///
    /// ```
    /// use kevy_replicate::source::{FromOffset, ReplicationSource};
    ///
    /// let backlog = ReplicationSource::new(1 << 20); // assigns offset 0 next
    /// assert!(matches!(backlog.frames_from(1), Err(FromOffset::Future)));
    /// ```
    Future,
}

/// Bounded backlog of recent replicated mutations.
///
/// ```
/// use kevy_replicate::source::ReplicationSource;
///
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// let mut backlog = ReplicationSource::new(1 << 20);
/// assert_eq!(backlog.push_mutation(&set), 0);
/// assert_eq!(backlog.push_mutation(&set), 1);
/// assert_eq!((backlog.oldest_offset(), backlog.newest_offset()), (Some(0), Some(1)));
/// ```
#[derive(Debug)]
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
    ///
    /// # Panics
    ///
    /// Panics if `max_bytes == 0` — a zero budget could never hold
    /// even one frame, so it is a caller bug, not a runtime state.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1); // smaller than any frame
    /// backlog.push_mutation(&set);
    /// assert_eq!(backlog.len(), 1); // the newest frame is kept anyway
    /// assert!(backlog.buffered_bytes() > backlog.max_bytes());
    /// ```
    pub fn new(max_bytes: usize) -> Self {
        assert!(max_bytes > 0, "ReplicationSource max_bytes must be > 0");
        Self { next_offset: 0, bytes_in_buf: 0, max_bytes, buf: std::collections::VecDeque::new() }
    }

    /// Resume offset assignment at `next` (boot continuity from the
    /// feed sidecar). Only meaningful on an empty, freshly created
    /// source — asserts the backlog has no frames.
    ///
    /// # Panics
    ///
    /// Panics if the backlog already holds frames: renumbering live
    /// frames would corrupt every replica's ack bookkeeping.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// backlog.set_next_offset(1_000); // offsets continue from the last session
    /// assert_eq!(backlog.push_mutation(&set), 1_000);
    /// ```
    pub fn set_next_offset(&mut self, next: u64) {
        assert!(self.buf.is_empty(), "set_next_offset on non-empty backlog");
        self.next_offset = next;
    }

    /// The byte budget this source was created with.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    ///
    /// assert_eq!(ReplicationSource::new(64 << 20).max_bytes(), 64 << 20);
    /// ```
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Next offset this source would assign. Equal to one past the
    /// last assigned offset; equals `0` for a fresh source.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// assert_eq!(backlog.next_offset(), 0);
    /// backlog.push_mutation(&set);
    /// assert_eq!(backlog.next_offset(), 1);
    /// ```
    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    /// Lowest offset still in the backlog, or `None` if empty.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// assert_eq!(backlog.oldest_offset(), None);
    /// for _ in 0..3 {
    ///     backlog.push_mutation(&set);
    /// }
    /// backlog.drop_up_to(2);
    /// assert_eq!(backlog.oldest_offset(), Some(2));
    /// ```
    pub fn oldest_offset(&self) -> Option<u64> {
        self.buf.front().map(|f| f.offset)
    }

    /// Highest offset still in the backlog, or `None` if empty.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// assert_eq!(backlog.newest_offset(), None);
    /// backlog.push_mutation(&set);
    /// backlog.push_mutation(&set);
    /// assert_eq!(backlog.newest_offset(), Some(1));
    /// ```
    pub fn newest_offset(&self) -> Option<u64> {
        self.buf.back().map(|f| f.offset)
    }

    /// Total bytes occupied by frames currently in the backlog.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// backlog.push_mutation(&set);
    /// assert_eq!(backlog.buffered_bytes(), kevy_replicate::wire::encode_frame(0, &set).len());
    /// ```
    pub fn buffered_bytes(&self) -> usize {
        self.bytes_in_buf
    }

    /// Number of frames currently in the backlog.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// backlog.push_mutation(&set);
    /// backlog.push_mutation(&set);
    /// assert_eq!(backlog.len(), 2);
    /// ```
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the backlog has no frames.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// assert!(backlog.is_empty());
    /// backlog.push_mutation(&set);
    /// assert!(!backlog.is_empty());
    /// ```
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
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    ///
    /// // the reactor's borrowed argv goes straight in
    /// let request = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";
    /// let (argv, _) = kevy_resp::parse_command_borrowed(request)?.expect("a whole command");
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// assert_eq!(backlog.push_mutation(&argv), 0);
    /// assert_eq!(backlog.len(), 1);
    /// # Ok::<(), kevy_resp::ProtocolError>(())
    /// ```
    // missing_panics_doc: the eviction loop's expect pops a front the loop
    // guard just observed — unreachable, not a caller-facing panic condition.
    #[allow(clippy::missing_panics_doc)]
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
    /// shard tick to enforce a retention floor tighter
    /// than the raw byte budget; lets the backlog reclaim space
    /// for live frames once all consumers have advanced.
    ///
    /// No-op when `watermark <= oldest_offset()` (nothing to drop)
    /// or when the buffer is empty. Updates the internal byte
    /// accounting to stay consistent with the live buffer length.
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// for _ in 0..5 {
    ///     backlog.push_mutation(&set);
    /// }
    /// backlog.drop_up_to(3); // every replica has acked past offset 2
    /// assert_eq!((backlog.len(), backlog.oldest_offset()), (2, Some(3)));
    /// ```
    // missing_panics_doc: same front-of-loop expect rationale as
    // `push_mutation` — unreachable by the `while let` guard.
    #[allow(clippy::missing_panics_doc)]
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
    ///
    /// ```
    /// use kevy_replicate::source::ReplicationSource;
    ///
    /// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
    /// let mut backlog = ReplicationSource::new(1 << 20);
    /// for _ in 0..4 {
    ///     backlog.push_mutation(&set);
    /// }
    /// let offsets: Vec<u64> = backlog.frames_from(2)?.map(|f| f.offset).collect();
    /// assert_eq!(offsets, [2, 3]);
    /// assert_eq!(backlog.frames_from(4)?.count(), 0); // caught up
    /// # Ok::<(), kevy_replicate::source::FromOffset>(())
    /// ```
    pub fn frames_from(&self, from: u64) -> Result<FramesIter<'_>, FromOffset> {
        if from > self.next_offset {
            return Err(FromOffset::Future);
        }
        // from == next_offset → replica is exactly caught up; empty slice.
        if from == self.next_offset {
            return Ok(FramesIter { buf: &self.buf, cursor: self.buf.len() });
        }
        // from < next_offset: the requested frame either is still in
        // the backlog or was evicted. Empty buf with from < next_offset
        // means every frame ever pushed has been evicted — same TooOld
        // outcome as `from < oldest`. (Without this branch the function
        // returns an empty iterator and the streaming pump silently
        // stalls — an embed-replica restart test caught this.)
        match self.oldest_offset() {
            Some(oldest) if from < oldest => return Err(FromOffset::TooOld),
            None => return Err(FromOffset::TooOld),
            _ => {}
        }
        // Offsets are monotonic, so the start index is a binary search. The
        // comment here used to say exactly that — and then called
        // `iter().position(...)`, an O(B) walk of the whole backlog, on a path
        // that both FEED.READ and the replica stream take on every poll. A
        // consumer resuming from an old cursor paid for the entire backlog
        // before it saw its first frame.
        //
        // `VecDeque` is two contiguous runs, each individually sorted, so
        // `partition_point` on each half gives the answer in O(log B).
        let (a, b) = self.buf.as_slices();
        let start = match a.partition_point(|f| f.offset < from) {
            i if i < a.len() => i,
            _ => a.len() + b.partition_point(|f| f.offset < from),
        };
        Ok(FramesIter { buf: &self.buf, cursor: start })
    }
}

/// Iterator over backlog frames returned by [`ReplicationSource::frames_from`].
///
/// ```
/// use kevy_replicate::source::ReplicationSource;
///
/// let set = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]);
/// let mut backlog = ReplicationSource::new(1 << 20);
/// backlog.push_mutation(&set);
/// backlog.push_mutation(&set);
/// let mut stream = Vec::new(); // what a replica socket would receive
/// for frame in backlog.frames_from(0)? {
///     stream.extend_from_slice(&frame.bytes);
/// }
/// assert_eq!(stream.len(), backlog.buffered_bytes());
/// # Ok::<(), kevy_replicate::source::FromOffset>(())
/// ```
#[derive(Debug)]
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
#[path = "source_tests.rs"]
mod tests;
