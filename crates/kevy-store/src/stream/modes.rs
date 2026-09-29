//! Named arguments for the stream commands' flags: what a write does to
//! a missing stream, whether a group read waits for `XACK`, and whether
//! a claim counts as a delivery.

/// What a stream write does when its key does not exist: `XADD` creates
/// the stream unless `NOMKSTREAM`; `XGROUP CREATE` refuses unless
/// `MKSTREAM`.
///
/// ```
/// use kevy_store::{MissingStream, Store, XAddIdSpec};
/// let mut s = Store::new();
/// let fields = vec![(b"f".to_vec(), b"v".to_vec())];
/// assert_eq!(s.xadd(b"s", XAddIdSpec::AutoAll, fields.clone(), MissingStream::Refuse, 1).unwrap(), None);
/// assert!(s.xadd(b"s", XAddIdSpec::AutoAll, fields, MissingStream::Create, 1).unwrap().is_some());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MissingStream {
    /// Create the stream (`XADD`'s default, `XGROUP CREATE … MKSTREAM`).
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
    /// let id = s.xadd(b"new", XAddIdSpec::AutoAll, f, MissingStream::Create, 5)?;
    /// assert_eq!(id, Some(StreamId::new(5, 0)));
    /// assert!(s.xgroup_create(b"other", b"g", GroupCreateMode::AtCurrent, MissingStream::Create)?);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Create,
    /// Leave the key absent: `XADD … NOMKSTREAM` answers `None`,
    /// `XGROUP CREATE` without `MKSTREAM` answers
    /// [`StoreError::NoSuchKey`](crate::StoreError::NoSuchKey).
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
    /// assert_eq!(s.xadd(b"new", XAddIdSpec::AutoAll, f, MissingStream::Refuse, 5)?, None);
    /// let r = s.xgroup_create(b"new", b"g", GroupCreateMode::AtCurrent, MissingStream::Refuse);
    /// assert_eq!(r, Err(StoreError::NoSuchKey));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    Refuse,
}

/// Whether entries an `XREADGROUP` delivers wait in the group's pending
/// list for `XACK`.
///
/// ```
/// use kevy_store::{AckMode, GroupCreateMode, MissingStream, ReadGroupId, StreamId, Store, XAddIdSpec};
/// let mut s = Store::new();
/// let f = vec![(b"f".to_vec(), b"v".to_vec())];
/// s.xadd(b"s", XAddIdSpec::AutoAll, f, MissingStream::Create, 1).unwrap();
/// s.xgroup_create(b"s", b"g", GroupCreateMode::AtId(StreamId::MIN), MissingStream::Refuse).unwrap();
/// s.xreadgroup(b"s", b"g", b"c", ReadGroupId::New, None, AckMode::NoAck, 2).unwrap();
/// assert_eq!(s.stream_group_peek(b"s", b"g").unwrap().pending_count(), 0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AckMode {
    /// Delivered entries join the pending list until acknowledged.
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
    /// // the setup read used AckMode::Pending: both entries wait for XACK
    /// assert_eq!(s.stream_group_peek(b"s", b"g").unwrap().pending_count(), 2);
    /// assert_eq!(s.xack(b"s", b"g", &[StreamId::new(1, 0)])?, 1);
    /// assert_eq!(s.stream_group_peek(b"s", b"g").unwrap().pending_count(), 1);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    #[default]
    Pending,
    /// `NOACK`: delivery is the acknowledgement; nothing is left pending.
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
    /// let got = s.xreadgroup(b"s", b"g", b"bob", ReadGroupId::New, None, AckMode::NoAck, 200)?;
    /// assert_eq!(got.len(), 1);
    /// assert_eq!(s.stream_group_peek(b"s", b"g").unwrap().pending_count(), 2, "unchanged");
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    NoAck,
}

/// Whether a claim is a redelivery: a plain `XCLAIM` / `XAUTOCLAIM`
/// counts one more delivery and replies with the entries; `JUSTID` moves
/// ownership only, leaves the delivery count alone and replies with IDs.
///
/// ```
/// use kevy_store::{ClaimMode, XClaimOpts};
/// assert_eq!(XClaimOpts::default().mode, ClaimMode::Deliver);
/// assert_eq!(XClaimOpts::default().with_mode(ClaimMode::JustId).mode, ClaimMode::JustId);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ClaimMode {
    /// A redelivery: the delivery count goes up by one.
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
    /// let opts = XClaimOpts::default().with_mode(ClaimMode::Deliver);
    /// let got = s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &opts, 150)?;
    /// assert_eq!(got[0].1, [(b"f".to_vec(), b"v".to_vec())], "the entry body comes back");
    /// let e = s.stream_group_peek(b"s", b"g").unwrap().pending_entry(StreamId::new(1, 0)).unwrap();
    /// assert_eq!(e.delivery_count, 2);
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    #[default]
    Deliver,
    /// `JUSTID`: ownership moves, the delivery count stays.
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
    /// let opts = XClaimOpts::default().with_mode(ClaimMode::JustId);
    /// s.xclaim(b"s", b"g", b"bob", &[StreamId::new(1, 0)], &opts, 150)?;
    /// let e = s.stream_group_peek(b"s", b"g").unwrap().pending_entry(StreamId::new(1, 0)).unwrap();
    /// assert_eq!((e.consumer.as_slice(), e.delivery_count), (b"bob".as_slice(), 1));
    /// # Ok::<(), kevy_store::StoreError>(())
    /// ```
    JustId,
}
