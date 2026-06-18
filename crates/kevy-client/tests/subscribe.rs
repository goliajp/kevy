//! Integration round-trip: spin a tiny TCP mock that script-replies with
//! canned RESP frames, drive it with `Subscriber`, assert classification.
//!
//! Same pattern as crates/kevy-resp-client/tests/roundtrip.rs — keeps the
//! test self-contained (no real kevy server thread needed).

use kevy_client::{PubsubEvent, Subscriber};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Start a mock server that, after accepting one connection, reads bytes
/// until at least `expect_in_at_least` (covers the SUBSCRIBE write) and
/// then streams `reply_bytes` back in one chunk. Closes after lingering.
fn mock_server(expect_in_at_least: usize, reply_bytes: &'static [u8]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (started_tx, started_rx) = mpsc::channel();
    thread::spawn(move || {
        started_tx.send(()).unwrap();
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = vec![0u8; 1024];
        let mut total = 0;
        while total < expect_in_at_least {
            match sock.read(&mut buf) {
                Ok(n) if n > 0 => total += n,
                _ => break, // Ok(0) eof or Err — same handling
            }
        }
        let _ = sock.write_all(reply_bytes);
        thread::sleep(Duration::from_millis(50));
    });
    started_rx.recv().unwrap();
    port
}

/// SUBSCRIBE chan request bytes: `*2\r\n$9\r\nSUBSCRIBE\r\n$4\r\nchan\r\n` = 31 bytes.
const SUBSCRIBE_CHAN_REQ_LEN: usize = 31;

#[test]
fn open_subscribes_and_receives_subscribe_ack() {
    let port = mock_server(
        SUBSCRIBE_CHAN_REQ_LEN,
        b"*3\r\n$9\r\nsubscribe\r\n$4\r\nchan\r\n:1\r\n",
    );
    let mut sub = Subscriber::open(&format!("kevy://127.0.0.1:{port}"), &[b"chan"]).unwrap();
    let ev = sub.recv().unwrap();
    assert_eq!(
        ev,
        PubsubEvent::Subscribe {
            channel: b"chan".to_vec(),
            count: 1,
        }
    );
}

#[test]
fn message_frame_classified_with_payload() {
    // Server pushes: subscribe ack + one message frame, back-to-back.
    let port = mock_server(
        SUBSCRIBE_CHAN_REQ_LEN,
        b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n\
          *3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n",
    );
    let mut sub = Subscriber::open(&format!("kevy://127.0.0.1:{port}"), &[b"news"]).unwrap();
    // Drain the ack.
    let _ = sub.recv().unwrap();
    let ev = sub.recv().unwrap();
    assert_eq!(
        ev,
        PubsubEvent::Message {
            channel: b"news".to_vec(),
            payload: b"hello".to_vec(),
        }
    );
}

#[test]
fn psubscribe_then_pmessage_round_trip() {
    // PSUBSCRIBE news.*: `*2\r\n$10\r\nPSUBSCRIBE\r\n$6\r\nnews.*\r\n` = 34 bytes.
    let port = mock_server(
        34,
        b"*3\r\n$10\r\npsubscribe\r\n$6\r\nnews.*\r\n:1\r\n\
          *4\r\n$8\r\npmessage\r\n$6\r\nnews.*\r\n$9\r\nnews.tech\r\n$2\r\nhi\r\n",
    );
    let mut sub = Subscriber::connect(&format!("kevy://127.0.0.1:{port}")).unwrap();
    sub.psubscribe(&[b"news.*"]).unwrap();
    assert_eq!(
        sub.recv().unwrap(),
        PubsubEvent::Psubscribe {
            pattern: b"news.*".to_vec(),
            count: 1,
        }
    );
    assert_eq!(
        sub.recv().unwrap(),
        PubsubEvent::Pmessage {
            pattern: b"news.*".to_vec(),
            channel: b"news.tech".to_vec(),
            payload: b"hi".to_vec(),
        }
    );
}

#[test]
fn unsubscribe_with_nil_channel_classified_as_none() {
    let port = mock_server(
        SUBSCRIBE_CHAN_REQ_LEN,
        // The "no channels were subscribed" wire shape: nil bulk in the
        // channel slot. Issued after we send UNSUBSCRIBE without args.
        b"*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n",
    );
    let mut sub = Subscriber::open(&format!("kevy://127.0.0.1:{port}"), &[b"chan"]).unwrap();
    // After SUBSCRIBE chan, we ignore the (not-sent here) ack and
    // immediately ask the mock for its canned UNSUBSCRIBE-nil reply.
    let ev = sub.recv().unwrap();
    assert_eq!(
        ev,
        PubsubEvent::Unsubscribe {
            channel: None,
            count: 0,
        }
    );
}

#[test]
fn server_close_yields_unexpected_eof() {
    // Mock sends nothing and closes — recv() must surface EOF, not loop.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (started_tx, started_rx) = mpsc::channel();
    thread::spawn(move || {
        started_tx.send(()).unwrap();
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 1024];
        let _ = sock.read(&mut buf);
        drop(sock);
    });
    started_rx.recv().unwrap();
    let mut sub = Subscriber::open(&format!("kevy://127.0.0.1:{port}"), &[b"chan"]).unwrap();
    let err = sub.recv().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn malformed_frame_yields_invalid_data() {
    let port = mock_server(SUBSCRIBE_CHAN_REQ_LEN, b"!totally-bogus\r\n");
    let mut sub = Subscriber::open(&format!("kevy://127.0.0.1:{port}"), &[b"chan"]).unwrap();
    let err = sub.recv().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn unknown_pubsub_kind_yields_invalid_data() {
    // Well-formed RESP array, but a bogus kind field. Should not crash —
    // we classify it as InvalidData with a descriptive message.
    let port = mock_server(
        SUBSCRIBE_CHAN_REQ_LEN,
        b"*3\r\n$5\r\nbogus\r\n$1\r\nx\r\n:0\r\n",
    );
    let mut sub = Subscriber::open(&format!("kevy://127.0.0.1:{port}"), &[b"chan"]).unwrap();
    let err = sub.recv().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn read_timeout_blocks_recv() {
    // No write from server → recv() must respect set_read_timeout and
    // return rather than hang forever.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (started_tx, started_rx) = mpsc::channel();
    thread::spawn(move || {
        started_tx.send(()).unwrap();
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 1024];
        let _ = sock.read(&mut buf);
        // Hold the connection open for a bit so the client's timeout fires
        // before EOF would (otherwise the test would race the EOF path).
        thread::sleep(Duration::from_millis(500));
    });
    started_rx.recv().unwrap();
    let mut sub = Subscriber::open(&format!("kevy://127.0.0.1:{port}"), &[b"chan"]).unwrap();
    sub.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let err = sub.recv().unwrap_err();
    // Different platforms surface read-timeout as WouldBlock vs TimedOut.
    let k = err.kind();
    assert!(
        matches!(
            k,
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "unexpected kind: {k:?}"
    );
}
