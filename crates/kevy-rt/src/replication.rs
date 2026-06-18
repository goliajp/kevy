//! Per-shard replication accept + handshake glue.
//!
//! This module owns the [`ReplicaConn`] state machine and the [`Shard`]
//! methods that drive it: accepting a replica TCP connection on the
//! per-shard listener (`replication_port_base + id`), running the
//! handshake (`REPLICATE FROM <offset> ID <replica-id>` → `+ACK <offset>`),
//! and reacting to read/write readiness for live replicas.
//!
//! The frame format, source backlog, handshake parse, and slot table
//! are stones in `kevy-replicate`; this module is the cement that wires
//! them into the reactor loop.
//!
//! Lifecycle: `HandshakePending` → `AckSent` (after parse + `+ACK`
//! queued) → `Streaming { sent_offset }` (after `+ACK` drains —
//! per-iter pump in [`crate::replication_pump`] fills more frames) →
//! `Closed { replica_id, sent_offset }` (peer EOF / cap exceeded /
//! `TooOld`). The reactor reaps Closed conns once per iter; at reap
//! time, conns whose `replica_id` is `Some` are recorded into
//! [`Shard::slots`] (per T1.15) so a reconnect within
//! `reconnect_window_ms` is correlatable.

use kevy_replicate::handshake::{HandshakeError, encode_ack, parse_replicate_from};
use kevy_resp::{Argv, parse_command_into};
use kevy_sys::Socket;

/// One active replica connection on this shard.
///
/// Lives in `Shard::replicas`. See the module-level docs for the
/// lifecycle diagram.
pub struct ReplicaConn {
    /// Owning socket. Dropped when the conn is dropped (closes the fd).
    pub sock: Socket,
    /// Cached raw fd for poller bookkeeping.
    pub fd: i32,
    /// Input buffer — bytes pulled off the socket waiting to parse.
    pub input: Vec<u8>,
    /// Output buffer — bytes queued for write_all (handshake `+ACK`
    /// in this batch; streamed frames in T1.14).
    pub output: Vec<u8>,
    /// Cursor into `output`; the next `Socket::write` writes from
    /// `output[write_off..]` and advances on partial sends.
    pub write_off: usize,
    /// Lifecycle state — drives the reactor's dispatch decisions.
    pub state: ReplicaState,
    /// Peer's `(IPv4, port)` captured at accept time (T1.28.5).
    /// `(0.0.0.0, 0)` for the fallback path (peer-vanished-pre-
    /// getpeername) and for synthetic conns built in unit tests.
    /// Used by `tick_replication_view` to enrich the per-shard view
    /// the command layer reads for `ROLE` / `INFO replication`.
    pub peer: (std::net::Ipv4Addr, u16),
}

/// Replication conn lifecycle. See [`ReplicaConn`] doc for the
/// transition diagram.
///
/// `Debug` only — `SnapshotShipping` carries a `mpsc::Receiver`
/// (T1.23.5 background-serializer handle) which has no
/// `PartialEq`, so the previous `Eq`/`PartialEq` derives are gone.
/// Tests compare via `matches!` instead.
#[derive(Debug)]
pub enum ReplicaState {
    /// Pre-handshake: input accumulates until a full RESP command parses.
    HandshakePending,
    /// `+ACK` queued in `output`; once the buffer drains the conn
    /// transitions to [`Self::Streaming`].
    AckSent {
        /// Replica id from the handshake (passed to Streaming).
        replica_id: String,
        /// Offset the replica asked to resume from (passed to
        /// Streaming as the initial `sent_offset`).
        from_offset: u64,
    },
    /// Live: the streaming pump fills `output` from the source backlog
    /// every reactor iteration; the writability handler drains.
    Streaming {
        /// Replica id (kept for observability / future INFO reporting).
        replica_id: String,
        /// Next offset to send. After encoding a frame at offset K,
        /// advances to K + 1. In v1.18.0 this also serves as the
        /// assumed-acked offset (no replica→primary ACK channel yet;
        /// see module docs + T1.15 wiring + Phase 1.5 kevy-elect).
        sent_offset: u64,
    },
    /// Snapshot ship in progress (T1.23). At trigger time the
    /// reactor freezes a COW [`kevy_store::SnapshotView`] (O(n)
    /// shallow clone — ns/entry) and hands it to a worker thread
    /// that serializes via `kevy_persist::write_snapshot_to`. The
    /// worker `mpsc::send`s the full RDB bytes back via
    /// `serializing`; until the bytes arrive, `pump_snapshot_chunks`
    /// is a no-op (output backpressure handles client wait). Once
    /// received, the pump chunks `snapshot_buf[snapshot_off..]` per
    /// SNAPSHOT_CHUNK_MAX, then on completion pushes
    /// `+SNAPSHOT_END <ack_offset>` and transitions to Streaming
    /// with `sent_offset = ack_offset`.
    SnapshotShipping {
        /// Replica id (carried into Streaming on completion).
        replica_id: String,
        /// Offset the snapshot was taken at; equals the primary's
        /// `source.next_offset()` at snapshot-trigger time. After
        /// snapshot ship completes, becomes the new `sent_offset`.
        ack_offset: u64,
        /// `Some(rx)` while the worker thread is serializing the
        /// SnapshotView; `pump_snapshot_chunks` polls via
        /// `try_recv` each tick. Cleared (set to `None`) once the
        /// bytes arrive.
        serializing: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
        /// Serialized RDB bytes once the worker delivers them.
        /// Empty + `serializing.is_some()` ⇒ still waiting on the
        /// background serializer (T1.23.5).
        snapshot_buf: Vec<u8>,
        /// Cursor into `snapshot_buf` — bytes [0..snapshot_off) have
        /// been chunked into `output` already.
        snapshot_off: usize,
    },
    /// Terminal: handshake failed, output cap exceeded, peer EOF, or
    /// the source can't serve `sent_offset` (TooOld → would need a
    /// snapshot ship, which arrives at T1.22+). Reactor reaps on
    /// next dispatch — at reap time, any conn that had reached
    /// AckSent/Streaming gets recorded in [`crate::shard::Shard::slots`]
    /// (per T1.15) so a reconnect within `reconnect_window_ms` can be
    /// observed/correlated. `replica_id = None` means the conn closed
    /// before handshake completed (nothing to record).
    Closed {
        /// Handshake's replica id, if the conn ever reached AckSent.
        replica_id: Option<String>,
        /// Highest sent offset (== assumed-acked in v1.18 — no real
        /// replica ACK channel yet, see Phase 1.5 kevy-elect). `0`
        /// when the conn closed before reaching Streaming.
        sent_offset: u64,
    },
}

impl ReplicaConn {
    /// Wrap a freshly-accepted socket. The socket must already be
    /// non-blocking (set by the caller before adding to the poller).
    /// Peer addr defaults to `(0.0.0.0, 0)` — call
    /// [`Self::with_peer`] from the accept path to record the real
    /// peer.
    #[allow(dead_code)] // legacy two-arg accept; the accept path now uses with_peer
    pub fn new(sock: Socket) -> Self {
        Self::with_peer(sock, (std::net::Ipv4Addr::UNSPECIFIED, 0))
    }

    /// Wrap a freshly-accepted socket together with its peer
    /// `(IPv4, port)` (captured by `Socket::peer_addr` at accept
    /// time; T1.28.5). Used by the replication listener so
    /// `tick_replication_view` can ship the per-replica address
    /// list to the command layer.
    pub fn with_peer(sock: Socket, peer: (std::net::Ipv4Addr, u16)) -> Self {
        let fd = sock.raw();
        Self {
            sock,
            fd,
            input: Vec::with_capacity(256),
            output: Vec::with_capacity(64),
            write_off: 0,
            state: ReplicaState::HandshakePending,
            peer,
        }
    }

    /// Transition to [`ReplicaState::Closed`] while preserving the
    /// replica id + sent offset the conn had at the moment of close.
    /// Idempotent. The reactor's reap step reads these fields to
    /// record the slot in [`crate::shard::Shard::slots`] before
    /// dropping the conn (T1.15).
    pub fn close(&mut self) {
        let (id, off) = match &self.state {
            ReplicaState::HandshakePending => (None, 0),
            ReplicaState::AckSent { replica_id, from_offset } => {
                (Some(replica_id.clone()), *from_offset)
            }
            ReplicaState::Streaming { replica_id, sent_offset } => {
                (Some(replica_id.clone()), *sent_offset)
            }
            ReplicaState::SnapshotShipping { replica_id, ack_offset, .. } => {
                // Snapshot was in flight; on reconnect within the
                // window the replica should retry — record the slot
                // at the snapshot's ack_offset so future `INFO
                // replication` / observability can see where we were.
                (Some(replica_id.clone()), *ack_offset)
            }
            ReplicaState::Closed { .. } => return,
        };
        self.state = ReplicaState::Closed {
            replica_id: id,
            sent_offset: off,
        };
    }
}

/// Try to advance the conn's handshake state. Pulls a single complete
/// RESP command out of `conn.input` (if one is there), runs the
/// handshake parser, and either pushes `+ACK` to `conn.output` or
/// returns the rejection reason for the caller to log + drop the conn.
///
/// Splits the handshake state machine from the I/O loop so this can be
/// unit-tested without standing up a Shard (see the module tests).
pub(crate) fn advance_handshake(conn: &mut ReplicaConn) -> Result<(), HandshakeError> {
    if !matches!(conn.state, ReplicaState::HandshakePending) {
        return Ok(());
    }
    let mut argv = Argv::default();
    let parsed = parse_command_into(&conn.input, &mut argv)
        .map_err(|_| HandshakeError::BadCommand)?;
    let consumed = match parsed {
        Some(n) => n,
        None => return Ok(()), // need more bytes — caller will read more
    };
    let req = parse_replicate_from(&argv)?;
    conn.input.drain(..consumed);
    conn.output.extend_from_slice(&encode_ack(req.from_offset));
    conn.write_off = 0;
    conn.state = ReplicaState::AckSent {
        replica_id: req.replica_id,
        from_offset: req.from_offset,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a ReplicaConn with no real socket — for handshake-only
    /// state-machine tests. Uses an invalid fd that would error on any
    /// real I/O call (the tests only exercise advance_handshake +
    /// state inspection, never read/write the socket).
    fn fake_conn() -> ReplicaConn {
        // SAFETY: the tests below only inspect state + buffers and call
        // advance_handshake; they never read/write/close this socket.
        // fd = -1 makes any accidental I/O call return EBADF rather
        // than silently corrupting an unrelated descriptor.
        let sock = unsafe { Socket::from_raw_fd(-1) };
        ReplicaConn {
            sock,
            fd: -1,
            input: Vec::new(),
            output: Vec::new(),
            write_off: 0,
            state: ReplicaState::HandshakePending,
            peer: (std::net::Ipv4Addr::UNSPECIFIED, 0),
        }
    }

    fn resp_replicate_from(offset: &str, id: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"*5\r\n");
        for arg in [b"REPLICATE".as_slice(), b"FROM", offset.as_bytes(), b"ID", id.as_bytes()] {
            v.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
            v.extend_from_slice(arg);
            v.extend_from_slice(b"\r\n");
        }
        v
    }

    #[test]
    fn close_from_handshake_pending_carries_no_id() {
        let mut conn = fake_conn();
        conn.close();
        match conn.state {
            ReplicaState::Closed { replica_id, sent_offset } => {
                assert_eq!(replica_id, None);
                assert_eq!(sent_offset, 0);
            }
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn close_from_ack_sent_preserves_id_and_offset() {
        let mut conn = fake_conn();
        conn.input = resp_replicate_from("17", "replica-x");
        advance_handshake(&mut conn).expect("handshake ok");
        conn.close();
        match conn.state {
            ReplicaState::Closed { replica_id, sent_offset } => {
                assert_eq!(replica_id.as_deref(), Some("replica-x"));
                assert_eq!(sent_offset, 17);
            }
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn close_from_streaming_preserves_id_and_offset() {
        let mut conn = fake_conn();
        conn.state = ReplicaState::Streaming {
            replica_id: "replica-z".into(),
            sent_offset: 99,
        };
        conn.close();
        match conn.state {
            ReplicaState::Closed { replica_id, sent_offset } => {
                assert_eq!(replica_id.as_deref(), Some("replica-z"));
                assert_eq!(sent_offset, 99);
            }
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn close_is_idempotent() {
        let mut conn = fake_conn();
        conn.state = ReplicaState::Streaming {
            replica_id: "r".into(),
            sent_offset: 5,
        };
        conn.close();
        let snapshot = format!("{:?}", conn.state);
        conn.close(); // second call must not overwrite fields
        assert_eq!(format!("{:?}", conn.state), snapshot);
    }

    #[test]
    fn handshake_pending_to_ack_sent_on_complete_command() {
        let mut conn = fake_conn();
        conn.input = resp_replicate_from("42", "replica-a");
        advance_handshake(&mut conn).expect("ok");
        match &conn.state {
            ReplicaState::AckSent { replica_id, from_offset } => {
                assert_eq!(replica_id, "replica-a");
                assert_eq!(*from_offset, 42);
            }
            other => panic!("expected AckSent, got {other:?}"),
        }
        assert_eq!(conn.output, b"+ACK 42\r\n");
        // Input fully consumed.
        assert!(conn.input.is_empty());
    }

    #[test]
    fn partial_handshake_stays_pending_and_waits_for_more_bytes() {
        let mut conn = fake_conn();
        let full = resp_replicate_from("0", "replica-a");
        // Hand over only the first half of the command.
        conn.input = full[..full.len() / 2].to_vec();
        advance_handshake(&mut conn).expect("ok");
        assert!(matches!(conn.state, ReplicaState::HandshakePending));
        assert!(conn.output.is_empty());
        // Append the rest — handshake completes.
        conn.input.extend_from_slice(&full[full.len() / 2..]);
        advance_handshake(&mut conn).expect("ok");
        assert!(matches!(conn.state, ReplicaState::AckSent { .. }));
    }

    #[test]
    fn wrong_command_is_rejected_at_handshake() {
        let mut conn = fake_conn();
        // Valid RESP, wrong verb.
        conn.input = b"*1\r\n$4\r\nPING\r\n".to_vec();
        let err = advance_handshake(&mut conn).unwrap_err();
        assert!(matches!(err, HandshakeError::WrongArity(_) | HandshakeError::BadCommand));
        // State stays HandshakePending; the caller marks Closed on err.
        assert!(matches!(conn.state, ReplicaState::HandshakePending));
    }

    #[test]
    fn inline_form_parses_then_handshake_rejects_arity() {
        // kevy-resp falls back to inline parsing on a non-`*` first
        // byte, so `!garbage\r\n` parses as a 1-arg argv `["!garbage"]`.
        // The RESP parser does NOT reject; the handshake layer does,
        // via WrongArity. Pins down which layer is responsible.
        let mut conn = fake_conn();
        conn.input = b"!garbage\r\n".to_vec();
        let err = advance_handshake(&mut conn).unwrap_err();
        assert_eq!(err, HandshakeError::WrongArity(1));
    }

    #[test]
    fn resp_level_malformed_input_returns_bad_command() {
        // Force the RESP multi-bulk parser into an error path —
        // `*1\r\n` claims one bulk arg but the body starts with `!`
        // instead of `$`. parse_command_into returns ProtocolError;
        // advance_handshake maps it to BadCommand.
        let mut conn = fake_conn();
        conn.input = b"*1\r\n!nope\r\n".to_vec();
        let err = advance_handshake(&mut conn).unwrap_err();
        assert_eq!(err, HandshakeError::BadCommand);
    }

    #[test]
    fn second_call_after_ack_is_noop() {
        let mut conn = fake_conn();
        conn.input = resp_replicate_from("7", "r");
        advance_handshake(&mut conn).unwrap();
        let out_before = conn.output.clone();
        // Calling again with empty input must not re-emit the ACK.
        advance_handshake(&mut conn).unwrap();
        assert_eq!(conn.output, out_before);
    }
}
