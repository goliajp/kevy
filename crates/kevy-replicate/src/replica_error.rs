//! Error surface of [`crate::replica::ReplicaClient`] — split from
//! `replica.rs` so each file stays under the 500-LOC house rule.
//! Re-exported from [`crate::replica`], so caller paths are
//! unchanged.

use crate::wire::WireError;
use std::io;

/// Errors a replica client can surface to its driver loop.
#[derive(Debug)]
pub enum ReplicaError {
    /// Primary closed the connection or never replied during the
    /// handshake / `+ACK` exchange.
    HandshakeRejected,
    /// `+ACK` line was malformed (didn't start with `+ACK `, didn't
    /// parse the offset).
    AckMalformed,
    /// Peer closed the connection mid-frame; reconnect to resume.
    Truncated,
    /// Wire-level decode error (envelope shape wrong, payload
    /// malformed, etc.).
    Frame(WireError),
    /// Frame arrived with an offset other than the expected next.
    /// Caller should trigger a full snapshot resync.
    OffsetGap {
        /// The offset the client expected next (= `last_seen + 1`).
        expected: u64,
        /// The offset the primary actually sent.
        got: u64,
    },
    /// While streaming a snapshot, the primary sent bytes that were
    /// neither a snapshot chunk nor `+SNAPSHOT_END`. Interleaving live
    /// frames inside a snapshot is forbidden (see `docs/snapshot.md`).
    UnexpectedInSnapshot,
    /// `next_frame` was called but the next event is a snapshot
    /// marker / chunk. Callers that want the snapshot-aware surface
    /// must use [`ReplicaClient::next_event`].
    SnapshotInProgress,
    /// Underlying socket I/O failure.
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

impl std::error::Error for ReplicaError {}

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
