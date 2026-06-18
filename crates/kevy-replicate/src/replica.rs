//! Replica-side client — connect to a primary's replication listener,
//! perform the handshake, then yield decoded mutation frames in order.
//!
//! The client is **synchronous + blocking** by design: it slots into a
//! dedicated thread on the replica node alongside (but separate from)
//! the regular kevy reactor. An async surface is a Phase 4 deliverable
//! (`kevy-client-async`, the only crate carved out of the 0-dep rule).
//!
//! Hot loop usage:
//!
//! ```no_run
//! use kevy_replicate::replica::ReplicaClient;
//!
//! let mut client = ReplicaClient::connect("127.0.0.1:16004", "replica-a", 0)
//!     .expect("connect ok");
//! while let Some(result) = client.next() {
//!     let frame = result.expect("decode ok");
//!     // apply frame.argv at frame.offset — caller's responsibility (T1.19)
//!     drop(frame);
//! }
//! ```
//!
//! Errors map to actionable next steps for the caller:
//! - [`ReplicaError::HandshakeRejected`] / [`ReplicaError::AckMalformed`]
//!   — primary refused or replied with garbage; drop the link, log,
//!   maybe back off and retry.
//! - [`ReplicaError::Truncated`] — peer EOF mid-frame; treat as a
//!   disconnect, reconnect later.
//! - [`ReplicaError::OffsetGap { expected, got }`] — frames arrived
//!   out of order or with a skip; per plan T1.20 the caller should
//!   trigger a full snapshot resync. v1.18.0 surfaces the gap; the
//!   snapshot ship machinery itself lands at T1.22.
//! - [`ReplicaError::Frame`] — wire-level decode error; same
//!   action as Truncated (drop + reconnect).

use crate::wire::WireError;
use kevy_resp::Argv;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A decoded mutation frame the replica should apply to its local
/// store. Ownership of the [`Argv`] passes to the caller.
#[derive(Debug)]
pub struct DecodedFrame {
    /// Monotonic offset the primary assigned at apply-time.
    pub offset: u64,
    /// Wire-decoded argv — feed to the dispatcher the same way AOF
    /// replay does (cmd name + arg bytes).
    pub argv: Argv,
}

/// Event yielded by [`ReplicaClient::next_event`]. A driver loop
/// pattern-matches and applies each:
/// - [`Self::Frame`] → run through the local dispatcher.
/// - [`Self::SnapshotBegin`] → caller should reset / prepare the
///   local store for a fresh-from-snapshot fill.
/// - [`Self::SnapshotChunk`] → append the bytes to the caller's
///   accumulating snapshot buffer.
/// - [`Self::SnapshotEnd`] → caller hands the accumulated buffer to
///   `kevy_persist::load_snapshot`; [`ReplicaClient`] has already
///   advanced `expected_offset` to `ack_offset`, so the next
///   [`Self::Frame`] arrives at `ack_offset` with no gap.
#[derive(Debug)]
pub enum ReplicaEvent {
    /// A live mutation frame.
    Frame(DecodedFrame),
    /// Snapshot ship begin marker (`+SNAPSHOT\r\n`).
    SnapshotBegin,
    /// One snapshot chunk's payload bytes (RESP bulk string body).
    SnapshotChunk(Vec<u8>),
    /// Snapshot ship end marker carrying the offset the next live
    /// frame will have.
    SnapshotEnd {
        /// The offset the primary's `next_offset` was at when the
        /// snapshot started. After this event, [`ReplicaClient::expected_offset`]
        /// equals this value.
        ack_offset: u64,
    },
}

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
    /// Caller should trigger a full snapshot resync (T1.22).
    OffsetGap {
        /// The offset the client expected next (= `last_seen + 1`).
        expected: u64,
        /// The offset the primary actually sent.
        got: u64,
    },
    /// While streaming a snapshot, the primary sent bytes that were
    /// neither a snapshot chunk nor `+SNAPSHOT_END`. v1.18.0 forbids
    /// interleaving live frames inside a snapshot (see `docs/snapshot.md`).
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

/// One blocking TCP connection to a primary's per-shard replication
/// listener. After [`Self::connect`] completes the handshake, the
/// client behaves as an `Iterator<Item = Result<DecodedFrame, ReplicaError>>`
/// yielding frames in offset order until the peer disconnects or a
/// hard error surfaces.
pub struct ReplicaClient {
    pub(crate) sock: TcpStream,
    /// Bytes pulled off the socket waiting to parse the next frame.
    pub(crate) buf: Vec<u8>,
    /// Position into `buf` where the next decode attempt starts. We
    /// drain `buf` only when this passes a high-water mark, so per-
    /// frame work avoids repeated `Vec::drain` shifts.
    pub(crate) cursor: usize,
    /// Offset the primary advertised at handshake (`+ACK <N>` value).
    /// Currently informational; T1.20 / T1.22 use it for gap-detection
    /// decisions (re-handshake vs full sync).
    pub(crate) primary_offset_at_handshake: u64,
    /// The next offset we expect from the stream. Initially the
    /// `from_offset` we requested; advances by 1 on each accepted frame.
    pub(crate) expected_offset: u64,
    /// `true` while we're between `+SNAPSHOT` and `+SNAPSHOT_END`.
    /// In this state, only chunk + end-marker bytes are valid; a
    /// `*2\r\n` (live frame envelope) returns
    /// [`ReplicaError::UnexpectedInSnapshot`] per the v1.18 spec
    /// (`docs/snapshot.md` — interleaving is a T1.25 extension).
    pub(crate) in_snapshot: bool,
}

impl ReplicaClient {
    /// Connect to `addr`, send `REPLICATE FROM <from_offset> ID <replica_id>`,
    /// read the `+ACK <offset>` reply, and return a ready-to-iterate
    /// client. Blocks until the handshake completes or the connect
    /// times out (`connect_timeout` argument).
    pub fn connect<A: ToSocketAddrs>(
        addr: A,
        replica_id: &str,
        from_offset: u64,
    ) -> Result<Self, ReplicaError> {
        Self::connect_with_timeout(addr, replica_id, from_offset, Duration::from_secs(5))
    }

    /// [`Self::connect`] with an explicit connect timeout. Useful for
    /// tests that don't want to wait the default 5 s when a port is
    /// closed.
    pub fn connect_with_timeout<A: ToSocketAddrs>(
        addr: A,
        replica_id: &str,
        from_offset: u64,
        connect_timeout: Duration,
    ) -> Result<Self, ReplicaError> {
        // Resolve + connect with timeout. ToSocketAddrs returns an
        // iterator; we try each address until one succeeds.
        let mut last_err: Option<io::Error> = None;
        let mut sock: Option<TcpStream> = None;
        for sa in addr.to_socket_addrs().map_err(ReplicaError::Io)? {
            match TcpStream::connect_timeout(&sa, connect_timeout) {
                Ok(s) => {
                    sock = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let mut sock = sock.ok_or_else(|| {
            ReplicaError::Io(last_err.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "no socket address resolved")
            }))
        })?;

        // Send the handshake. `encode_replicate_from` is a private
        // helper so the on-the-wire shape is one place to change.
        let req = encode_replicate_from(from_offset, replica_id);
        sock.write_all(&req)?;

        // Read the `+ACK <offset>\r\n` reply. Use a small read timeout
        // so a primary that opens the socket but never replies doesn't
        // hang the replica forever.
        sock.set_read_timeout(Some(connect_timeout))?;
        let primary_offset = read_ack(&mut sock)?;
        // Clear the read timeout for normal streaming (replica may sit
        // for minutes with no frames if the primary is idle).
        sock.set_read_timeout(None)?;
        sock.set_nonblocking(false)?; // explicit: blocking reads after handshake.

        Ok(ReplicaClient {
            sock,
            buf: Vec::with_capacity(8 * 1024),
            cursor: 0,
            primary_offset_at_handshake: primary_offset,
            expected_offset: from_offset,
            in_snapshot: false,
        })
    }

    /// Offset the primary reported at handshake (`+ACK <N>` value).
    /// Informational — exposed so callers can log + future T1.22
    /// snapshot-ship logic can compare against the local applied
    /// offset to decide resume vs full-sync.
    pub fn primary_offset_at_handshake(&self) -> u64 {
        self.primary_offset_at_handshake
    }

    /// Return a `try_clone`'d handle on the underlying socket. The
    /// clone shares the same kernel file description, so calling
    /// `shutdown(Shutdown::Both)` on it unblocks any in-flight
    /// blocking read on the original (and vice versa). T1.29.5 uses
    /// this to interrupt a runner thread parked in `next_event` when
    /// `REPLICAOF` retargets or `REPLICAOF NO ONE` demotes — without
    /// this handle, the runner stays blocked until the upstream peer
    /// closes the connection.
    pub fn socket_handle(&self) -> io::Result<TcpStream> {
        self.sock.try_clone()
    }

    /// The offset the next frame should carry. Advances on every
    /// successful `next()`.
    pub fn expected_offset(&self) -> u64 {
        self.expected_offset
    }

    /// Pull the next frame from the stream. Frame-only convenience —
    /// returns [`ReplicaError::SnapshotInProgress`] if the primary is
    /// sending a snapshot. Callers that need the snapshot-aware
    /// surface (T1.22) must use [`Self::next_event`] instead.
    /// Returns `None` on clean peer EOF (no buffered bytes left).
    pub fn next_frame(&mut self) -> Option<Result<DecodedFrame, ReplicaError>> {
        match self.next_event()? {
            Ok(ReplicaEvent::Frame(f)) => Some(Ok(f)),
            Ok(_) => Some(Err(ReplicaError::SnapshotInProgress)),
            Err(e) => Some(Err(e)),
        }
    }

    /// Drop already-consumed prefix when the cursor has walked past
    /// 4 KiB of buffer (amortises per-frame work without doing a full
    /// `drain` on every frame). Used by the event-decoding helpers
    /// in [`crate::replica_decode`].
    pub(crate) fn maybe_compact_buf(&mut self) {
        if self.cursor >= 4 * 1024 {
            self.buf.drain(..self.cursor);
            self.cursor = 0;
        }
    }
}

impl Iterator for ReplicaClient {
    type Item = Result<DecodedFrame, ReplicaError>;
    /// Frame-only iterator. Use [`ReplicaClient::next_event`] for the
    /// snapshot-aware surface.
    fn next(&mut self) -> Option<Self::Item> {
        self.next_frame()
    }
}

/// Compose a `REPLICATE FROM <offset> ID <id>` RESP2 multi-bulk
/// request — symmetric to `handshake::parse_replicate_from` on the
/// primary side.
fn encode_replicate_from(from_offset: u64, replica_id: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(64 + replica_id.len());
    v.extend_from_slice(b"*5\r\n");
    let offset_str = from_offset.to_string();
    for arg in [
        b"REPLICATE".as_slice(),
        b"FROM",
        offset_str.as_bytes(),
        b"ID",
        replica_id.as_bytes(),
    ] {
        let header = format!("${}\r\n", arg.len());
        v.extend_from_slice(header.as_bytes());
        v.extend_from_slice(arg);
        v.extend_from_slice(b"\r\n");
    }
    v
}

/// Read `+ACK <offset>\r\n` from `sock`, return the parsed offset.
/// Pulls one byte at a time — the reply is < 30 bytes, so the per-
/// byte syscall cost is negligible and avoids a buffering surface
/// we'd have to thread into the client struct just for the handshake.
fn read_ack(sock: &mut TcpStream) -> Result<u64, ReplicaError> {
    let mut line = Vec::with_capacity(32);
    let mut b = [0u8; 1];
    loop {
        match sock.read(&mut b) {
            Ok(0) => return Err(ReplicaError::HandshakeRejected),
            Ok(_) => {
                line.push(b[0]);
                if line.len() >= 2 && line.ends_with(b"\r\n") {
                    break;
                }
                if line.len() > 256 {
                    return Err(ReplicaError::AckMalformed);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(ReplicaError::Io(e)),
        }
    }
    parse_ack_line(&line)
}

fn parse_ack_line(line: &[u8]) -> Result<u64, ReplicaError> {
    let body = line.strip_suffix(b"\r\n").ok_or(ReplicaError::AckMalformed)?;
    let body = body.strip_prefix(b"+ACK ").ok_or(ReplicaError::AckMalformed)?;
    let s = std::str::from_utf8(body).map_err(|_| ReplicaError::AckMalformed)?;
    s.parse::<u64>().map_err(|_| ReplicaError::AckMalformed)
}

#[cfg(test)]
impl ReplicaClient {
    /// Test-only constructor that wraps an already-connected socket
    /// without doing the handshake. Lets unit tests drive the event
    /// loop against canned bytes from the other end of a TcpStream pair.
    pub(crate) fn from_socket_for_test(sock: TcpStream, expected_offset: u64) -> Self {
        Self {
            sock,
            buf: Vec::with_capacity(8 * 1024),
            cursor: 0,
            primary_offset_at_handshake: expected_offset,
            expected_offset,
            in_snapshot: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_replicate_from_matches_what_primary_parses() {
        // Round-trip: encode here, parse via the primary-side parser.
        let bytes = encode_replicate_from(42, "replica-a");
        let mut argv = Argv::default();
        let consumed = kevy_resp::parse_command_into(&bytes, &mut argv)
            .expect("parse ok")
            .expect("complete");
        assert_eq!(consumed, bytes.len());
        let req = crate::handshake::parse_replicate_from(&argv).expect("handshake ok");
        assert_eq!(req.from_offset, 42);
        assert_eq!(req.replica_id, "replica-a");
    }

    #[test]
    fn ack_line_parses_offsets() {
        assert_eq!(parse_ack_line(b"+ACK 0\r\n").unwrap(), 0);
        assert_eq!(parse_ack_line(b"+ACK 42\r\n").unwrap(), 42);
        assert_eq!(parse_ack_line(b"+ACK 12345678\r\n").unwrap(), 12_345_678);
    }

    #[test]
    fn ack_line_rejects_malformed() {
        assert!(matches!(
            parse_ack_line(b"+PONG\r\n"),
            Err(ReplicaError::AckMalformed)
        ));
        assert!(matches!(
            parse_ack_line(b"+ACK abc\r\n"),
            Err(ReplicaError::AckMalformed)
        ));
        assert!(matches!(
            parse_ack_line(b"-ERR nope\r\n"),
            Err(ReplicaError::AckMalformed)
        ));
        // Missing CRLF.
        assert!(matches!(
            parse_ack_line(b"+ACK 1"),
            Err(ReplicaError::AckMalformed)
        ));
    }

    #[test]
    fn ack_line_rejects_offset_overflow() {
        // 21+ digits — beyond u64::MAX. parse::<u64>() returns Err →
        // AckMalformed.
        assert!(matches!(
            parse_ack_line(b"+ACK 99999999999999999999999\r\n"),
            Err(ReplicaError::AckMalformed)
        ));
    }

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
