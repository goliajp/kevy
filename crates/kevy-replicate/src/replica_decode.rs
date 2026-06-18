//! Snapshot-aware event decoding for [`crate::replica::ReplicaClient`]
//! — split from `replica.rs` to keep that file under the 500-LOC
//! project ceiling. The state-machine helpers live here; the type
//! definitions, `connect`, and `next_frame` stay in `replica.rs`.

use crate::replica::{DecodedFrame, ReplicaClient, ReplicaError, ReplicaEvent};
use crate::wire::{
    SnapshotMarker, WireError, decode_frame, decode_snapshot_chunk, decode_snapshot_marker,
};
use std::io::{self, Read};

impl ReplicaClient {
    /// Snapshot-aware iterator step. Returns one [`ReplicaEvent`] per
    /// call — a live `Frame`, a `SnapshotBegin`/`SnapshotChunk`/
    /// `SnapshotEnd`, or one of the [`ReplicaError`] variants.
    /// Returns `None` on clean peer EOF.
    ///
    /// Snapshot bookkeeping:
    /// - Entering `SnapshotBegin` sets `in_snapshot = true`; chunk
    ///   bytes are valid until `SnapshotEnd`.
    /// - `SnapshotEnd { ack_offset }` sets `expected_offset =
    ///   ack_offset` (so the next live `Frame` has no gap) and
    ///   clears `in_snapshot`.
    /// - Live `*2\r\n` bytes during a snapshot return
    ///   [`ReplicaError::UnexpectedInSnapshot`] (v1.18 forbids
    ///   interleaving — see `docs/snapshot.md`).
    pub fn next_event(&mut self) -> Option<Result<ReplicaEvent, ReplicaError>> {
        loop {
            if let Some(result) = self.try_decode_one_event() {
                return Some(result);
            }
            // Need more bytes off the socket.
            let mut chunk = [0u8; 4096];
            match self.sock.read(&mut chunk) {
                Ok(0) => {
                    if self.cursor < self.buf.len() {
                        return Some(Err(ReplicaError::Truncated));
                    }
                    return None;
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Some(Err(ReplicaError::Io(e))),
            }
        }
    }

    /// Try to decode one event from the buffered bytes. Returns
    /// `None` when more bytes are needed (the loop in [`Self::next_event`]
    /// will read more). Split out so the outer loop stays tiny + the
    /// per-event dispatch fits the project's 50-LOC-fn rule.
    fn try_decode_one_event(&mut self) -> Option<Result<ReplicaEvent, ReplicaError>> {
        if self.cursor >= self.buf.len() {
            return None;
        }
        let first = self.buf[self.cursor];
        match first {
            b'+' => self.try_decode_snapshot_marker(),
            b'$' if self.in_snapshot => self.try_decode_snapshot_chunk(),
            b'*' if self.in_snapshot => {
                Some(Err(ReplicaError::UnexpectedInSnapshot))
            }
            b'*' => self.try_decode_live_frame(),
            _ => Some(Err(ReplicaError::Frame(WireError::BadEnvelope))),
        }
    }

    fn try_decode_live_frame(&mut self) -> Option<Result<ReplicaEvent, ReplicaError>> {
        match decode_frame(&self.buf[self.cursor..]) {
            Ok((offset, argv, used)) => {
                self.cursor += used;
                self.maybe_compact_buf();
                if offset != self.expected_offset {
                    return Some(Err(ReplicaError::OffsetGap {
                        expected: self.expected_offset,
                        got: offset,
                    }));
                }
                self.expected_offset = self.expected_offset.saturating_add(1);
                Some(Ok(ReplicaEvent::Frame(DecodedFrame { offset, argv })))
            }
            Err(WireError::Truncated) => None,
            Err(e) => Some(Err(ReplicaError::Frame(e))),
        }
    }

    fn try_decode_snapshot_marker(&mut self) -> Option<Result<ReplicaEvent, ReplicaError>> {
        match decode_snapshot_marker(&self.buf[self.cursor..]) {
            Ok(Some((SnapshotMarker::Begin, used))) => {
                self.cursor += used;
                self.maybe_compact_buf();
                self.in_snapshot = true;
                Some(Ok(ReplicaEvent::SnapshotBegin))
            }
            Ok(Some((SnapshotMarker::End(ack_offset), used))) => {
                self.cursor += used;
                self.maybe_compact_buf();
                self.in_snapshot = false;
                self.expected_offset = ack_offset;
                Some(Ok(ReplicaEvent::SnapshotEnd { ack_offset }))
            }
            Ok(None) => Some(Err(ReplicaError::Frame(WireError::BadEnvelope))),
            Err(WireError::Truncated) => None,
            Err(e) => Some(Err(ReplicaError::Frame(e))),
        }
    }

    fn try_decode_snapshot_chunk(&mut self) -> Option<Result<ReplicaEvent, ReplicaError>> {
        match decode_snapshot_chunk(&self.buf[self.cursor..]) {
            Ok((chunk, used)) => {
                let owned = chunk.to_vec();
                self.cursor += used;
                self.maybe_compact_buf();
                Some(Ok(ReplicaEvent::SnapshotChunk(owned)))
            }
            Err(WireError::Truncated) => None,
            Err(e) => Some(Err(ReplicaError::Frame(e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::replica::{ReplicaClient, ReplicaError, ReplicaEvent};
    use crate::wire::{encode_frame, encode_snapshot_begin, encode_snapshot_chunk, encode_snapshot_end};
    use kevy_resp::Argv;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    fn argv_for(args: &[&[u8]]) -> Argv {
        let mut a = Argv::default();
        for arg in args {
            a.push(arg);
        }
        a
    }

    #[test]
    fn next_event_snapshot_path_begin_chunks_end_then_frame() {
        let (mut srv, cli) = tcp_pair();
        thread::spawn(move || {
            srv.write_all(&encode_snapshot_begin()).unwrap();
            srv.write_all(&encode_snapshot_chunk(b"hello-snapshot")).unwrap();
            srv.write_all(&encode_snapshot_chunk(b"more-snapshot-bytes")).unwrap();
            srv.write_all(&encode_snapshot_end(42)).unwrap();
            srv.write_all(&encode_frame(42, &argv_for(&[b"SET", b"k", b"v"]))).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(srv);
        });
        let mut client = ReplicaClient::from_socket_for_test(cli, 0);

        assert!(matches!(client.next_event(), Some(Ok(ReplicaEvent::SnapshotBegin))));
        match client.next_event() {
            Some(Ok(ReplicaEvent::SnapshotChunk(bytes))) => {
                assert_eq!(bytes, b"hello-snapshot");
            }
            other => panic!("expected SnapshotChunk, got {other:?}"),
        }
        match client.next_event() {
            Some(Ok(ReplicaEvent::SnapshotChunk(bytes))) => {
                assert_eq!(bytes, b"more-snapshot-bytes");
            }
            other => panic!("expected SnapshotChunk, got {other:?}"),
        }
        match client.next_event() {
            Some(Ok(ReplicaEvent::SnapshotEnd { ack_offset })) => assert_eq!(ack_offset, 42),
            other => panic!("expected SnapshotEnd, got {other:?}"),
        }
        assert_eq!(client.expected_offset(), 42);
        match client.next_event() {
            Some(Ok(ReplicaEvent::Frame(f))) => {
                assert_eq!(f.offset, 42);
                assert_eq!(f.argv, argv_for(&[b"SET", b"k", b"v"]));
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn next_event_live_frame_during_snapshot_is_unexpected() {
        let (mut srv, cli) = tcp_pair();
        thread::spawn(move || {
            srv.write_all(&encode_snapshot_begin()).unwrap();
            srv.write_all(&encode_snapshot_chunk(b"first")).unwrap();
            srv.write_all(&encode_frame(0, &argv_for(&[b"PING"]))).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(srv);
        });
        let mut client = ReplicaClient::from_socket_for_test(cli, 0);
        assert!(matches!(client.next_event(), Some(Ok(ReplicaEvent::SnapshotBegin))));
        assert!(matches!(client.next_event(), Some(Ok(ReplicaEvent::SnapshotChunk(_)))));
        assert!(matches!(
            client.next_event(),
            Some(Err(ReplicaError::UnexpectedInSnapshot))
        ));
    }

    #[test]
    fn next_frame_returns_snapshot_in_progress_when_snapshot_starts() {
        let (mut srv, cli) = tcp_pair();
        thread::spawn(move || {
            srv.write_all(&encode_snapshot_begin()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(srv);
        });
        let mut client = ReplicaClient::from_socket_for_test(cli, 0);
        assert!(matches!(
            client.next_frame(),
            Some(Err(ReplicaError::SnapshotInProgress))
        ));
    }

    #[test]
    fn live_frame_path_via_next_event() {
        let (mut srv, cli) = tcp_pair();
        thread::spawn(move || {
            srv.write_all(&encode_frame(0, &argv_for(&[b"SET", b"a", b"1"]))).unwrap();
            srv.write_all(&encode_frame(1, &argv_for(&[b"SET", b"b", b"2"]))).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(srv);
        });
        let mut client = ReplicaClient::from_socket_for_test(cli, 0);
        for expected_off in 0..2 {
            match client.next_event() {
                Some(Ok(ReplicaEvent::Frame(f))) => assert_eq!(f.offset, expected_off),
                other => panic!("expected Frame {expected_off}, got {other:?}"),
            }
        }
        assert_eq!(client.expected_offset(), 2);
    }

    #[test]
    fn snapshot_end_with_zero_offset_handled() {
        let (mut srv, cli) = tcp_pair();
        thread::spawn(move || {
            srv.write_all(&encode_snapshot_begin()).unwrap();
            srv.write_all(&encode_snapshot_end(0)).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(srv);
        });
        let mut client = ReplicaClient::from_socket_for_test(cli, 0);
        assert!(matches!(client.next_event(), Some(Ok(ReplicaEvent::SnapshotBegin))));
        match client.next_event() {
            Some(Ok(ReplicaEvent::SnapshotEnd { ack_offset })) => assert_eq!(ack_offset, 0),
            other => panic!("expected SnapshotEnd, got {other:?}"),
        }
        assert_eq!(client.expected_offset(), 0);
    }
}
