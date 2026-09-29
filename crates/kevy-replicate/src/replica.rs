//! Replica-side client — connect to a primary's replication listener,
//! perform the handshake, then yield decoded mutation frames in order.
//!
//! The client is **synchronous + blocking** by design: it slots into a
//! dedicated thread on the replica node alongside (but separate from)
//! the regular kevy reactor.
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
//!     // apply frame.argv at frame.offset — caller's responsibility
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
//!   out of order or with a skip; the caller should trigger a full
//!   snapshot resync.
//! - [`ReplicaError::Frame`] — wire-level decode error; same
//!   action as Truncated (drop + reconnect).

use crate::feed::FeedPosition;
pub use crate::replica_error::ReplicaError;
use kevy_resp::Argv;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};

pub use crate::replica_connect::ConnectOptions;
#[cfg(feature = "secure")]
pub use crate::replica_secure::ReplicaSecurity;
use std::time::Duration;

/// A decoded mutation frame the replica should apply to its local
/// store. Ownership of the [`Argv`] passes to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DecodedFrame {
    /// Monotonic offset the primary assigned at apply-time.
    pub offset: u64,
    /// Wire-decoded argv — feed to the dispatcher the same way AOF
    /// replay does (cmd name + arg bytes).
    pub argv: Argv,
}

impl DecodedFrame {
    /// The frame at `offset` carrying `argv`.
    ///
    /// ```
    /// use kevy_replicate::replica::DecodedFrame;
    ///
    /// let frame = DecodedFrame::new(7, kevy_resp::Argv::from(vec![b"DEL".to_vec(), b"k".to_vec()]));
    /// assert_eq!(frame.offset, 7);
    /// ```
    pub fn new(offset: u64, argv: Argv) -> Self {
        Self { offset, argv }
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReplicaEvent {
    /// A live mutation frame.
    Frame(DecodedFrame),
    /// In-stream heartbeat: the primary's tail at send time — its feed
    /// generation (the REPL.TOKEN / REPL.WAIT gen truth; `0` = the
    /// primary spoke the legacy one-number heartbeat, "unknown") and its
    /// `next_offset`. Lets the replica compute lag (applied vs primary)
    /// and judge link liveness. Occupies no offset space.
    Ping(FeedPosition),
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

/// One blocking TCP connection to a primary's per-shard replication
/// listener. After [`Self::connect`] completes the handshake, the
/// client behaves as an `Iterator<Item = Result<DecodedFrame, ReplicaError>>`
/// yielding frames in offset order until the peer disconnects or a
/// hard error surfaces.
#[derive(Debug)]
pub struct ReplicaClient {
    pub(crate) sock: TcpStream,
    /// Bytes pulled off the socket waiting to parse the next frame.
    pub(crate) buf: Vec<u8>,
    /// Position into `buf` where the next decode attempt starts. We
    /// drain `buf` only when this passes a high-water mark, so per-
    /// frame work avoids repeated `Vec::drain` shifts.
    pub(crate) cursor: usize,
    /// What the primary advertised at handshake (`+ACK <gen> <N>`).
    /// Whatever this session delivers (frames or snapshot) belongs to
    /// its generation — the caller records it as its data's generation
    /// once the delivery lands, and presents it on the next reconnect.
    /// The offset is informational; useful for gap-detection decisions
    /// (re-handshake vs full sync).
    pub(crate) primary_at_handshake: FeedPosition,
    /// The next offset we expect from the stream. Initially the
    /// `from_offset` we requested; advances by 1 on each accepted frame.
    pub(crate) expected_offset: u64,
    /// `true` while we're between `+SNAPSHOT` and `+SNAPSHOT_END`.
    /// In this state, only chunk + end-marker bytes are valid; a
    /// `*2\r\n` (live frame envelope) returns
    /// [`ReplicaError::UnexpectedInSnapshot`] — interleaving live
    /// frames inside a snapshot is forbidden (`docs/snapshot.md`).
    pub(crate) in_snapshot: bool,
    /// `Some` on a Noise-protected link.
    pub(crate) noise: Option<crate::replica_secure::ClientNoise>,
}

impl ReplicaClient {
    /// Connect to `addr` with no continuity claim (generation 0),
    /// send `REPLICATE FROM 0 <from_offset> ID <replica_id>`, read
    /// the `+ACK <gen> <offset>` reply, and return a ready-to-iterate
    /// client. Blocks until the handshake completes or 5 s pass.
    /// Callers resuming with data from a prior session must use
    /// [`Self::connect_with`] and present that data's generation — a
    /// gen-0 claim with a nonzero offset makes the primary ship a
    /// snapshot rather than risk offset aliasing.
    pub fn connect<A: ToSocketAddrs>(
        addr: A,
        replica_id: &str,
        from_offset: u64,
    ) -> Result<Self, ReplicaError> {
        Self::connect_with(
            addr,
            &ConnectOptions::new(replica_id).with_from(FeedPosition::new(0, from_offset)),
        )
    }

    /// The plaintext half of [`Self::connect_with`].
    pub(crate) fn connect_plain<A: ToSocketAddrs>(
        addr: A,
        opts: &ConnectOptions,
    ) -> Result<Self, ReplicaError> {
        let connect_timeout = opts.timeout;
        let replica_id = opts.replica_id.as_str();
        let mut sock = connect_stream(addr, connect_timeout)?;

        // Send the handshake. `encode_replicate_from` is a private
        // helper so the on-the-wire shape is one place to change.
        let req = encode_replicate_from(opts.from, replica_id);
        sock.write_all(&req)?;

        // Read the `+ACK <gen> <offset>\r\n` reply. Use a small read
        // timeout so a primary that opens the socket but never
        // replies doesn't hang the replica forever.
        sock.set_read_timeout(Some(connect_timeout))?;
        let primary_at_handshake = read_ack(&mut sock)?;
        // Clear the read timeout for normal streaming (replica may sit
        // for minutes with no frames if the primary is idle).
        sock.set_read_timeout(None)?;
        sock.set_nonblocking(false)?; // explicit: blocking reads after handshake.

        Ok(ReplicaClient {
            sock,
            buf: Vec::with_capacity(8 * 1024),
            cursor: 0,
            primary_at_handshake,
            expected_offset: opts.from.offset,
            in_snapshot: false,
            noise: None,
        })
    }

    /// The position the primary reported at handshake (`+ACK <gen> <N>`).
    ///
    /// Everything this session delivers belongs to its generation; a
    /// heartbeat carrying a DIFFERENT generation mid-session means the
    /// primary broke continuity under us (FLUSHALL / promotion) — the
    /// caller should drop the link and re-handshake so the fence decides
    /// afresh. The offset is informational — exposed so callers can log,
    /// and so snapshot-ship logic can compare against the local applied
    /// offset to decide resume vs full-sync.
    ///
    /// ```no_run
    /// use kevy_replicate::replica::ReplicaClient;
    ///
    /// let client = ReplicaClient::connect("127.0.0.1:16004", "replica-a", 0)?;
    /// println!("primary at generation {}", client.primary_at_handshake().generation);
    /// # Ok::<(), kevy_replicate::replica::ReplicaError>(())
    /// ```
    pub fn primary_at_handshake(&self) -> FeedPosition {
        self.primary_at_handshake
    }

    /// Return a `try_clone`'d handle on the underlying socket. The
    /// clone shares the same kernel file description, so calling
    /// `shutdown(Shutdown::Both)` on it unblocks any in-flight
    /// blocking read on the original (and vice versa). The server uses
    /// this to interrupt a runner thread parked in `next_event` when
    /// `REPLICAOF` retargets or `REPLICAOF NO ONE` demotes — without
    /// this handle, the runner stays blocked until the upstream peer
    /// closes the connection.
    pub fn socket_handle(&self) -> io::Result<TcpStream> {
        self.sock.try_clone()
    }

    /// Write `REPLCONF ACK <offset>` back on the replication
    /// connection. The primary's pump drains these non-blocking and
    /// advances the replica's slot; call every ~100ms with the highest
    /// received frame offset + 1 (i.e. the next offset you expect).
    pub fn send_ack(&mut self, offset: u64) -> std::io::Result<()> {
        use std::io::Write as _;
        let ack = crate::wire::encode_replconf_ack(offset);
        match self.noise.as_mut() {
            Some(n) => n.write(&mut self.sock, &ack),
            None => self.sock.write_all(&ack),
        }
    }

    /// The offset the next frame should carry. Advances on every
    /// successful `next()`.
    pub fn expected_offset(&self) -> u64 {
        self.expected_offset
    }

    /// Pull the next frame from the stream. Frame-only convenience —
    /// returns [`ReplicaError::SnapshotInProgress`] if the primary is
    /// sending a snapshot. Callers that need the snapshot-aware
    /// surface must use [`Self::next_event`] instead.
    /// Returns `None` on clean peer EOF (no buffered bytes left).
    pub fn next_frame(&mut self) -> Option<Result<DecodedFrame, ReplicaError>> {
        loop {
            match self.next_event()? {
                Ok(ReplicaEvent::Frame(f)) => return Some(Ok(f)),
                // Heartbeats are out-of-band — invisible to a
                // frame-only consumer.
                Ok(ReplicaEvent::Ping { .. }) => {}
                Ok(_) => return Some(Err(ReplicaError::SnapshotInProgress)),
                Err(e) => return Some(Err(e)),
            }
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

/// Resolve + connect with timeout. `ToSocketAddrs` returns an
/// iterator; try each address until one succeeds.
pub(crate) fn connect_stream<A: ToSocketAddrs>(
    addr: A,
    connect_timeout: Duration,
) -> Result<TcpStream, ReplicaError> {
    let mut last_err: Option<io::Error> = None;
    for sa in addr.to_socket_addrs().map_err(ReplicaError::Io)? {
        match TcpStream::connect_timeout(&sa, connect_timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    Err(ReplicaError::Io(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "no socket address resolved")
    })))
}

/// Compose a `REPLICATE FROM <gen> <offset> ID <id>` RESP2
/// multi-bulk request — symmetric to
/// `HandshakeReq::parse` on the primary side.
pub(crate) fn encode_replicate_from(from: FeedPosition, replica_id: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(80 + replica_id.len());
    v.extend_from_slice(b"*6\r\n");
    let gen_str = from.generation.to_string();
    let offset_str = from.offset.to_string();
    for arg in [
        b"REPLICATE".as_slice(),
        b"FROM",
        gen_str.as_bytes(),
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

/// Read `+ACK <gen> <offset>\r\n` from `sock`, return the parsed
/// position.
/// Pulls one byte at a time — the reply is < 50 bytes, so the per-
/// byte syscall cost is negligible and avoids a buffering surface
/// we'd have to thread into the client struct just for the handshake.
fn read_ack(sock: &mut TcpStream) -> Result<FeedPosition, ReplicaError> {
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
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ReplicaError::Io(e)),
        }
    }
    parse_ack_line(&line)
}

pub(crate) fn parse_ack_line(line: &[u8]) -> Result<FeedPosition, ReplicaError> {
    let body = line.strip_suffix(b"\r\n").ok_or(ReplicaError::AckMalformed)?;
    let body = body.strip_prefix(b"+ACK ").ok_or(ReplicaError::AckMalformed)?;
    let s = std::str::from_utf8(body).map_err(|_| ReplicaError::AckMalformed)?;
    // Exactly two space-separated decimals: `<gen> <offset>`. A
    // one-number (pre-4.0) ACK is malformed — clean wire break.
    let (gen_s, off_s) = s.split_once(' ').ok_or(ReplicaError::AckMalformed)?;
    let generation = gen_s.parse::<u64>().map_err(|_| ReplicaError::AckMalformed)?;
    let offset = off_s.parse::<u64>().map_err(|_| ReplicaError::AckMalformed)?;
    Ok(FeedPosition::new(generation, offset))
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
            primary_at_handshake: FeedPosition::new(1, expected_offset),
            expected_offset,
            in_snapshot: false,
            noise: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_replicate_from_matches_what_primary_parses() {
        // Round-trip: encode here, parse via the primary-side parser.
        let bytes = encode_replicate_from(FeedPosition::new(3, 42), "replica-a");
        let mut argv = Argv::default();
        let consumed =
            kevy_resp::parse_command_into(&bytes, &mut argv).expect("parse ok").expect("complete");
        assert_eq!(consumed, bytes.len());
        let req = crate::handshake::HandshakeReq::parse(&argv).expect("handshake ok");
        assert_eq!(req.from, FeedPosition::new(3, 42));
        assert_eq!(req.replica_id, "replica-a");
    }

    #[test]
    fn ack_line_parses_gen_and_offset() {
        assert_eq!(parse_ack_line(b"+ACK 1 0\r\n").unwrap(), FeedPosition::new(1, 0));
        assert_eq!(parse_ack_line(b"+ACK 7 42\r\n").unwrap(), FeedPosition::new(7, 42));
        assert_eq!(
            parse_ack_line(b"+ACK 2 12345678\r\n").unwrap(),
            FeedPosition::new(2, 12_345_678)
        );
    }

    #[test]
    fn ack_line_rejects_malformed() {
        assert!(matches!(parse_ack_line(b"+PONG\r\n"), Err(ReplicaError::AckMalformed)));
        assert!(matches!(parse_ack_line(b"+ACK abc 1\r\n"), Err(ReplicaError::AckMalformed)));
        assert!(matches!(parse_ack_line(b"-ERR nope\r\n"), Err(ReplicaError::AckMalformed)));
        // The legacy one-number (pre-4.0) ACK — clean wire break.
        assert!(matches!(parse_ack_line(b"+ACK 42\r\n"), Err(ReplicaError::AckMalformed)));
        // Missing CRLF.
        assert!(matches!(parse_ack_line(b"+ACK 1 1"), Err(ReplicaError::AckMalformed)));
    }

    #[test]
    fn ack_line_rejects_offset_overflow() {
        // 21+ digits — beyond u64::MAX. parse::<u64>() returns Err →
        // AckMalformed.
        assert!(matches!(
            parse_ack_line(b"+ACK 1 99999999999999999999999\r\n"),
            Err(ReplicaError::AckMalformed)
        ));
    }
}
