//! Error surface of [`crate::replica::ReplicaClient`] — split from
//! `replica.rs` so each file stays under the 500-LOC house rule.
//! Re-exported from [`crate::replica`], so caller paths are
//! unchanged.

use crate::wire::WireError;
use std::io;

/// Errors a replica client can surface to its driver loop.
///
/// ```
/// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
/// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
///
/// # let stream = kevy_replicate::wire::encode_frame(3, &argv(&["SET", "k", "v"]));
/// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", stream);
/// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
/// match client.next_event() {
///     Some(Err(ReplicaError::OffsetGap { .. })) => { /* resync through a snapshot */ }
///     Some(Err(e)) => return Err(e.into()), // drop the link and reconnect
///     other => panic!("unexpected {other:?}"),
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum ReplicaError {
    /// Primary closed the connection or never replied during the
    /// handshake / `+ACK` exchange.
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
    ///
    /// # let (addr, _primary) = fake_primary(b"", Vec::new());
    /// // the primary hangs up without an `+ACK`
    /// let err = ReplicaClient::connect(addr, "replica-a", 0).unwrap_err();
    /// assert!(matches!(err, ReplicaError::HandshakeRejected));
    /// ```
    HandshakeRejected,
    /// `+ACK` line was malformed (didn't start with `+ACK `, didn't
    /// parse the offset).
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
    ///
    /// # let (addr, _primary) = fake_primary(b"-ERR not a primary\r\n", Vec::new());
    /// let err = ReplicaClient::connect(addr, "replica-a", 0).unwrap_err();
    /// assert!(matches!(err, ReplicaError::AckMalformed));
    /// ```
    AckMalformed,
    /// Peer closed the connection mid-frame; reconnect to resume.
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
    ///
    /// # let frame = kevy_replicate::wire::encode_frame(0, &argv(&["SET", "k", "v"]));
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", frame[..frame.len() - 3].to_vec());
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
    /// assert!(matches!(client.next_event(), Some(Err(ReplicaError::Truncated))));
    /// # Ok::<(), ReplicaError>(())
    /// ```
    Truncated,
    /// Wire-level decode error (envelope shape wrong, payload
    /// malformed, etc.).
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
    /// use kevy_replicate::wire::WireError;
    ///
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", b":1\r\n".to_vec());
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
    /// let event = client.next_event();
    /// assert!(matches!(event, Some(Err(ReplicaError::Frame(WireError::BadEnvelope)))));
    /// # Ok::<(), ReplicaError>(())
    /// ```
    Frame(WireError),
    /// Frame arrived with an offset other than the expected next.
    /// Caller should trigger a full snapshot resync.
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
    ///
    /// # let stream = kevy_replicate::wire::encode_frame(12, &argv(&["SET", "k", "v"]));
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 10\r\n", stream);
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 10)?;
    /// let Some(Err(ReplicaError::OffsetGap { expected, got })) = client.next_event() else { panic!() };
    /// assert_eq!((expected, got), (10, 12)); // frames 10 and 11 never arrived
    /// # Ok::<(), ReplicaError>(())
    /// ```
    OffsetGap {
        /// The offset the client expected next (= `last_seen + 1`).
        ///
        /// ```
        /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
        /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
        ///
        /// # let stream = kevy_replicate::wire::encode_frame(12, &argv(&["SET", "k", "v"]));
        /// # let (addr, _primary) = fake_primary(b"+ACK 1 10\r\n", stream);
        /// let mut client = ReplicaClient::connect(addr, "replica-a", 10)?;
        /// let Some(Err(ReplicaError::OffsetGap { expected, .. })) = client.next_event() else {
        ///     panic!("expected a gap")
        /// };
        /// assert_eq!(expected, 10);
        /// # Ok::<(), ReplicaError>(())
        /// ```
        expected: u64,
        /// The offset the primary actually sent.
        ///
        /// ```
        /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
        /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
        ///
        /// # let stream = kevy_replicate::wire::encode_frame(12, &argv(&["SET", "k", "v"]));
        /// # let (addr, _primary) = fake_primary(b"+ACK 1 10\r\n", stream);
        /// let mut client = ReplicaClient::connect(addr, "replica-a", 10)?;
        /// let Some(Err(ReplicaError::OffsetGap { got, .. })) = client.next_event() else {
        ///     panic!("expected a gap")
        /// };
        /// assert_eq!(got, 12);
        /// # Ok::<(), ReplicaError>(())
        /// ```
        got: u64,
    },
    /// While streaming a snapshot, the primary sent bytes that were
    /// neither a snapshot chunk nor `+SNAPSHOT_END`. Interleaving live
    /// frames inside a snapshot is forbidden (see `docs/snapshot.md`).
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
    /// use kevy_replicate::wire;
    ///
    /// # let mut stream = wire::encode_snapshot_begin();
    /// # stream.extend(wire::encode_frame(0, &argv(&["SET", "k", "v"])));
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", stream);
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
    /// client.next_event(); // SnapshotBegin
    /// assert!(matches!(client.next_event(), Some(Err(ReplicaError::UnexpectedInSnapshot))));
    /// # Ok::<(), ReplicaError>(())
    /// ```
    UnexpectedInSnapshot,
    /// `next_frame` was called but the next event is a snapshot
    /// marker / chunk. Callers that want the snapshot-aware surface
    /// must use [`crate::replica::ReplicaClient::next_event`].
    ///
    /// ```
    /// # mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/doc_primary.rs")); } use doc::*;
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
    ///
    /// # let (addr, _primary) = fake_primary(b"+ACK 1 0\r\n", kevy_replicate::wire::encode_snapshot_begin());
    /// let mut client = ReplicaClient::connect(addr, "replica-a", 0)?;
    /// assert!(matches!(client.next_frame(), Some(Err(ReplicaError::SnapshotInProgress))));
    /// # Ok::<(), ReplicaError>(())
    /// ```
    SnapshotInProgress,
    /// Underlying socket I/O failure.
    ///
    /// ```
    /// use kevy_replicate::replica::{ReplicaClient, ReplicaError};
    ///
    /// // nothing listens on a port whose listener was just dropped
    /// let addr = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    /// let err = ReplicaClient::connect(addr, "replica-a", 0).unwrap_err();
    /// let ReplicaError::Io(io) = err else { panic!("expected an I/O error, got {err}") };
    /// assert_eq!(io.kind(), std::io::ErrorKind::ConnectionRefused);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    Io(io::Error),
}

impl std::fmt::Display for ReplicaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HandshakeRejected => write!(f, "primary rejected replication handshake"),
            Self::AckMalformed => write!(f, "primary sent malformed +ACK"),
            Self::Truncated => write!(f, "replication stream truncated by peer"),
            Self::Frame(e) => write!(f, "replication frame decode error: {e}"),
            Self::OffsetGap { expected, got } => {
                write!(f, "replication offset gap: expected {expected}, got {got}")
            }
            Self::UnexpectedInSnapshot => {
                write!(f, "primary sent non-chunk bytes mid-snapshot")
            }
            Self::SnapshotInProgress => {
                write!(f, "snapshot in progress; use next_event() to consume")
            }
            Self::Io(e) => write!(f, "replication socket I/O error: {e}"),
        }
    }
}

impl std::error::Error for ReplicaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Frame(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ReplicaError {
    fn from(e: io::Error) -> Self {
        ReplicaError::Io(e)
    }
}

impl From<WireError> for ReplicaError {
    fn from(e: WireError) -> Self {
        match e {
            WireError::Truncated => ReplicaError::Truncated,
            other => ReplicaError::Frame(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_io_error_wraps_into_io_variant() {
        let e: ReplicaError = io::Error::new(io::ErrorKind::ConnectionRefused, "x").into();
        assert!(matches!(e, ReplicaError::Io(_)));
    }

    #[test]
    fn from_wire_error_truncated_maps_to_truncated() {
        let e: ReplicaError = WireError::Truncated.into();
        assert!(matches!(e, ReplicaError::Truncated));
    }

    #[test]
    fn from_wire_error_other_maps_to_frame() {
        let e: ReplicaError = WireError::BadEnvelope.into();
        assert!(matches!(e, ReplicaError::Frame(_)));
    }
}
