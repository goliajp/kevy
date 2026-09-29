//! The value types of a consumer group: pending entries, per-consumer
//! counters, and the ID arguments of `XGROUP CREATE` / `XREADGROUP`.
//! Split from `group.rs` to keep it under the 500-LOC cap.

use super::StreamId;
use crate::value::SmallBytes;

/// One pending entry: who got it, when, and how many times.
///
/// ```
/// # use kevy_store::*;
/// # let mut s = Store::new();
/// # for t in [1, 2] {
/// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
/// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
/// # }
/// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
/// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
/// let g = s.stream_group_peek(b"s", b"g").unwrap();
/// let e = g.pending_entry(StreamId::new(1, 0)).expect("pending");
/// assert_eq!((e.consumer.as_slice(), e.delivery_time_ms, e.delivery_count), (b"alice".as_slice(), 100, 1));
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PelEntry {
    /// Owning consumer's name. Used by XPENDING's `consumer` filter
    /// and XCLAIM's ownership transfer.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &XClaimOpts::default(), 150)?;
    /// let g = s.stream_group_peek(b"s", b"g").unwrap();
    /// assert_eq!(g.pending_entry(StreamId::new(1, 0)).unwrap().consumer.as_slice(), b"bob");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub consumer: SmallBytes,
    /// Last delivery wall-clock (unix-ms). XCLAIM compares idle =
    /// `now - delivery_time_ms` against its `min-idle-ms` arg.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// let g = s.stream_group_peek(b"s", b"g").unwrap();
    /// // delivered by the XREADGROUP at t=100
    /// assert_eq!(g.pending_entry(StreamId::new(2, 0)).unwrap().delivery_time_ms, 100);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub delivery_time_ms: u64,
    /// Number of times this entry has been delivered (=1 on first
    /// XREADGROUP, +=1 on each XCLAIM that doesn't have JUSTID).
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &XClaimOpts::default(), 150)?;
    /// let g = s.stream_group_peek(b"s", b"g").unwrap();
    /// assert_eq!(g.pending_entry(StreamId::new(1, 0)).unwrap().delivery_count, 2);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    pub delivery_count: u32,
}

/// Per-consumer cached counters so `XINFO CONSUMERS` answers in O(1).
/// Read through [`ConsumerGroup::consumer`](crate::ConsumerGroup::consumer) /
/// [`ConsumerGroup::consumers`](crate::ConsumerGroup::consumers).
///
/// ```
/// # use kevy_store::*;
/// # let mut s = Store::new();
/// # for t in [1, 2] {
/// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
/// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
/// # }
/// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
/// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
/// let g = s.stream_group_peek(b"s", b"g").unwrap();
/// let c = g.consumer(b"alice").expect("known consumer");
/// assert_eq!((c.name(), c.pending_count(), c.last_seen_ms()), (b"alice".as_slice(), 2, 100));
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Clone, Debug)]
pub struct ConsumerState {
    /// Consumer name.
    pub(crate) name: SmallBytes,
    /// Last wall-clock (unix-ms) the consumer interacted with the
    /// group (any XREADGROUP / XACK / XCLAIM touch).
    pub(crate) last_seen_ms: u64,
    /// Cached size of this consumer's slice of the PEL.
    pub(crate) pel_count: usize,
}

/// `XGROUP CREATE` ID argument: either an explicit ID or `$`
/// (= current stream's `last_id`, resolved by the caller).
///
/// ```
/// # use kevy_store::*;
/// # let mut s = Store::new();
/// # for t in [1, 2] {
/// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
/// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
/// # }
/// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
/// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
/// s.xgroup_create(b"s", b"late", GroupCreateMode::AtCurrent, MissingStream::Refuse)?;
/// let late = s.stream_group_peek(b"s", b"late").unwrap();
/// assert_eq!(late.last_delivered_id(), StreamId::new(2, 0));
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GroupCreateMode {
    /// `<ms>-<seq>` literal — the group's `last_delivered_id` starts here.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// let at = GroupCreateMode::AtId(StreamId::new(1, 0));
    /// s.xgroup_create(b"s", b"g2", at, MissingStream::Refuse)?;
    /// // only entries after 1-0 are new to this group
    /// let got = s.xreadgroup(b"s", b"g2", b"c", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// assert_eq!(got[0].0, StreamId::new(2, 0));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    AtId(StreamId),
    /// `$` — resolve to the stream's current `last_id` at create time.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// s.xgroup_create(b"s", b"g2", GroupCreateMode::AtCurrent, MissingStream::Refuse)?;
    /// let got = s.xreadgroup(b"s", b"g2", b"c", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// assert!(got.is_empty(), "nothing is newer than `$`");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    AtCurrent,
}

impl ConsumerState {
    /// The consumer's name.
    pub fn name(&self) -> &[u8] {
        self.name.as_slice()
    }
    /// `XINFO CONSUMERS`' `pending` field.
    pub fn pending_count(&self) -> usize {
        self.pel_count
    }
    /// Last unix-ms this consumer interacted with the group.
    pub fn last_seen_ms(&self) -> u64 {
        self.last_seen_ms
    }
}

/// XREADGROUP's per-stream ID: either `>` (= new entries) or an explicit
/// "after this id" for PEL replay.
///
/// ```
/// # use kevy_store::*;
/// # let mut s = Store::new();
/// # for t in [1, 2] {
/// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
/// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
/// # }
/// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
/// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
/// let replay = s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::ReplayAfter(StreamId::MIN), None, AckMode::Pending, 200)?;
/// assert_eq!(replay.len(), 2, "alice's pending entries");
/// # Ok::<(), kevy_store::StoreError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReadGroupId {
    /// `>` — new entries only.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, 3)?;
    /// let got = s.xreadgroup(b"s", b"g", b"bob", ReadGroupId::New, None, AckMode::Pending, 200)?;
    /// assert_eq!(got.len(), 1, "only the entry added after the last read");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    New,
    /// `<id>` — replay PEL entries strictly after this id.
    ///
    /// ```
    /// # use kevy_store::*;
    /// # let mut s = Store::new();
    /// # for t in [1, 2] {
    /// #     let f = vec![(b"f".to_vec(), b"v".to_vec())];
    /// #     s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, t)?;
    /// # }
    /// # s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse)?;
    /// # s.xreadgroup(b"s", b"g", b"alice", ReadGroupId::New, None, AckMode::Pending, 100)?;
    /// let r = ReadGroupId::ReplayAfter(StreamId::new(1, 0));
    /// let got = s.xreadgroup(b"s", b"g", b"alice", r, None, AckMode::Pending, 200)?;
    /// assert_eq!(got[0].0, StreamId::new(2, 0));
    /// // bob holds nothing, so his replay is empty
    /// let got = s.xreadgroup(b"s", b"g", b"bob", ReadGroupId::ReplayAfter(StreamId::MIN), None, AckMode::Pending, 200)?;
    /// assert!(got.is_empty());
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    ReplayAfter(StreamId),
}
