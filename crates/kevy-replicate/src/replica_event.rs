//! The events a [`crate::replica::ReplicaClient`] yields — split from
//! `replica.rs` so each file stays under the 500-LOC house rule.
//! Re-exported from [`crate::replica`], so caller paths are unchanged.

use crate::feed::FeedPosition;
use crate::replica::DecodedFrame;

/// Event yielded by [`ReplicaClient::next_event`](crate::replica::ReplicaClient::next_event). A driver loop
/// pattern-matches and applies each:
/// - [`Self::Frame`] → run through the local dispatcher.
/// - [`Self::SnapshotBegin`] → caller should reset / prepare the
///   local store for a fresh-from-snapshot fill.
/// - [`Self::SnapshotChunk`] → append the bytes to the caller's
///   accumulating snapshot buffer.
/// - [`Self::SnapshotEnd`] → caller hands the accumulated buffer to
///   `kevy_persist::load_snapshot`; [`ReplicaClient`](crate::replica::ReplicaClient) has already
///   advanced `expected_offset` to `ack_offset`, so the next
///   [`Self::Frame`] arrives at `ack_offset` with no gap.
///
/// A replica joining through a snapshot sees the whole sequence:
///
/// ```
/// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
/// use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};
/// use kevy_replicate::wire;
///
/// # let mut stream = wire::encode_snapshot_begin();
/// # stream.extend(wire::encode_snapshot_chunk(b"dump"));
/// # stream.extend(wire::encode_snapshot_end(9));
/// # stream.extend(wire::encode_frame(9, &argv(&["SET", "k", "v"])));
/// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", stream);
/// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
/// let mut snapshot = Vec::new();
/// while let Some(event) = client.next_event() {
///     match event? {
///         ReplicaEvent::SnapshotBegin => snapshot.clear(),
///         ReplicaEvent::SnapshotChunk(bytes) => snapshot.extend(bytes),
///         ReplicaEvent::SnapshotEnd { ack_offset } => assert_eq!(ack_offset, 9),
///         ReplicaEvent::Frame(frame) => assert_eq!(frame.offset, 9),
///         _ => {}
///     }
/// }
/// assert_eq!(snapshot, b"dump");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReplicaEvent {
    /// A live mutation frame.
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{DecodedFrame, ReplicaClient, ReplicaEvent};
    ///
    /// # let stream = kevy_replicate::wire::encode_frame(0, &argv(&["SET", "k", "v"]));
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", stream);
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
    /// let expected = DecodedFrame::new(0, argv(&["SET", "k", "v"]));
    /// assert_eq!(client.next_event().transpose()?, Some(ReplicaEvent::Frame(expected)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Frame(DecodedFrame),
    /// In-stream heartbeat: the primary's tail at send time — its feed
    /// generation (the REPL.TOKEN / REPL.WAIT gen truth; `0` = the
    /// primary spoke the legacy one-number heartbeat, "unknown") and its
    /// `next_offset`. Lets the replica compute lag (applied vs primary)
    /// and judge link liveness. Occupies no offset space.
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::feed::FeedPosition;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};
    ///
    /// # let stream = kevy_replicate::wire::encode_ping(FeedPosition::new(1, 120));
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 100\r\n", stream);
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 100)?;
    /// let Some(ReplicaEvent::Ping(tail)) = client.next_event().transpose()? else { panic!() };
    /// assert_eq!(tail.offset - client.expected_offset(), 20); // frames behind the primary
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Ping(FeedPosition),
    /// Snapshot ship begin marker (`+SNAPSHOT\r\n`).
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};
    ///
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", kevy_replicate::wire::encode_snapshot_begin());
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
    /// assert_eq!(client.next_event().transpose()?, Some(ReplicaEvent::SnapshotBegin));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    SnapshotBegin,
    /// One snapshot chunk's payload bytes (RESP bulk string body).
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};
    /// use kevy_replicate::wire;
    ///
    /// # let mut stream = wire::encode_snapshot_begin();
    /// # stream.extend(wire::encode_snapshot_chunk(b"part-1"));
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", stream);
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
    /// client.next_event(); // SnapshotBegin
    /// let chunk = ReplicaEvent::SnapshotChunk(b"part-1".to_vec());
    /// assert_eq!(client.next_event().transpose()?, Some(chunk));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    SnapshotChunk(Vec<u8>),
    /// Snapshot ship end marker carrying the offset the next live
    /// frame will have.
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};
    /// use kevy_replicate::wire;
    ///
    /// # let mut stream = wire::encode_snapshot_begin();
    /// # stream.extend(wire::encode_snapshot_end(57));
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", stream);
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
    /// client.next_event(); // SnapshotBegin
    /// let end = client.next_event().transpose()?;
    /// assert_eq!(end, Some(ReplicaEvent::SnapshotEnd { ack_offset: 57 }));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    SnapshotEnd {
        /// The offset the primary's `next_offset` was at when the
        /// snapshot started. After this event, [`ReplicaClient::expected_offset`](crate::replica::ReplicaClient::expected_offset)
        /// equals this value.
        ///
        /// ```
        /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
        /// use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};
        /// use kevy_replicate::wire;
        ///
        /// # let mut stream = wire::encode_snapshot_begin();
        /// # stream.extend(wire::encode_snapshot_end(57));
        /// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", stream);
        /// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
        /// client.next_event(); // SnapshotBegin
        /// let Some(ReplicaEvent::SnapshotEnd { ack_offset }) = client.next_event().transpose()? else {
        ///     panic!("expected the end marker")
        /// };
        /// assert_eq!(client.expected_offset(), ack_offset); // the next live frame is 57
        /// # Ok::<(), Box<dyn std::error::Error>>(())
        /// ```
        ack_offset: u64,
    },
}
