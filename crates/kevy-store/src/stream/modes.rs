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
    Create,
    /// Leave the key absent: `XADD … NOMKSTREAM` answers `None`,
    /// `XGROUP CREATE` without `MKSTREAM` answers
    /// [`StoreError::NoSuchKey`](crate::StoreError::NoSuchKey).
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
    #[default]
    Pending,
    /// `NOACK`: delivery is the acknowledgement; nothing is left pending.
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
    #[default]
    Deliver,
    /// `JUSTID`: ownership moves, the delivery count stays.
    JustId,
}
