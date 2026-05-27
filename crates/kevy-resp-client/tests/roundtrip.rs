//! Round-trip integration tests: spin a tiny TCP listener in a thread that
//! echoes back a canned RESP reply for each request, drive it with RespClient,
//! assert the parsed reply matches.

use kevy_resp::Reply;
use kevy_resp_client::RespClient;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Start a one-shot mock RESP server that, after accepting one connection,
/// reads `expect_in_at_least` bytes of request and responds with `reply_bytes`,
/// then closes. Returns the bound port.
fn mock_server(expect_in_at_least: usize, reply_bytes: &'static [u8]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (started_tx, started_rx) = mpsc::channel();
    thread::spawn(move || {
        started_tx.send(()).unwrap();
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        // Read until we've seen at least the expected request length (chunked
        // reads are fine; we just need to know "client sent request").
        let mut buf = vec![0u8; 1024];
        let mut total = 0;
        while total < expect_in_at_least {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(_) => break,
            }
        }
        let _ = sock.write_all(reply_bytes);
        // Linger so the client can read fully before close.
        thread::sleep(Duration::from_millis(50));
    });
    started_rx.recv().unwrap();
    port
}

#[test]
fn ping_pong_roundtrip() {
    // PING request bytes = *1\r\n$4\r\nPING\r\n (14 bytes)
    let port = mock_server(14, b"+PONG\r\n");
    let mut c = RespClient::connect("127.0.0.1", port).unwrap();
    let reply = c.request(&[b"PING".to_vec()]).unwrap();
    match reply {
        Reply::Simple(s) => assert_eq!(s, b"PONG"),
        other => panic!("expected Simple('PONG'), got {other:?}"),
    }
}

#[test]
fn get_returns_bulk_string() {
    // GET foo (multibulk; ≥ 17 bytes)
    let port = mock_server(17, b"$5\r\nhello\r\n");
    let mut c = RespClient::connect("127.0.0.1", port).unwrap();
    let reply = c.request(&[b"GET".to_vec(), b"foo".to_vec()]).unwrap();
    match reply {
        Reply::Bulk(b) => assert_eq!(b, b"hello"),
        other => panic!("expected Bulk('hello'), got {other:?}"),
    }
}

#[test]
fn missing_key_returns_nil() {
    let port = mock_server(17, b"$-1\r\n");
    let mut c = RespClient::connect("127.0.0.1", port).unwrap();
    let reply = c.request(&[b"GET".to_vec(), b"foo".to_vec()]).unwrap();
    assert!(matches!(reply, Reply::Nil));
}

#[test]
fn integer_reply() {
    let port = mock_server(17, b":42\r\n");
    let mut c = RespClient::connect("127.0.0.1", port).unwrap();
    let reply = c.request(&[b"INCR".to_vec(), b"x".to_vec()]).unwrap();
    assert!(matches!(reply, Reply::Int(42)));
}

#[test]
fn array_reply() {
    let port = mock_server(14, b"*2\r\n$1\r\na\r\n$1\r\nb\r\n");
    let mut c = RespClient::connect("127.0.0.1", port).unwrap();
    let reply = c.request(&[b"KEYS".to_vec()]).unwrap();
    match reply {
        Reply::Array(items) => {
            assert_eq!(items.len(), 2);
            assert!(matches!(&items[0], Reply::Bulk(b) if b == b"a"));
            assert!(matches!(&items[1], Reply::Bulk(b) if b == b"b"));
        }
        other => panic!("expected Array([Bulk('a'), Bulk('b')]), got {other:?}"),
    }
}

#[test]
fn error_reply() {
    let port = mock_server(14, b"-WRONGTYPE oops\r\n");
    let mut c = RespClient::connect("127.0.0.1", port).unwrap();
    let reply = c.request(&[b"GET".to_vec()]).unwrap();
    match reply {
        Reply::Error(b) => assert_eq!(b, b"WRONGTYPE oops"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn malformed_reply_yields_invalid_data_error() {
    // Server sends bytes that can NEVER be a valid RESP frame (unknown
    // type tag '!') — RespClient must surface ErrorKind::InvalidData,
    // not retry forever or yield UnexpectedEof.
    let port = mock_server(14, b"!garbage\r\n");
    let mut c = RespClient::connect("127.0.0.1", port).unwrap();
    let err = c.request(&[b"PING".to_vec()]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn server_close_mid_reply_yields_io_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (started_tx, started_rx) = mpsc::channel();
    thread::spawn(move || {
        started_tx.send(()).unwrap();
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 1024];
        let _ = sock.read(&mut buf);
        // Send a partial reply and close. RespClient should see UnexpectedEof.
        let _ = sock.write_all(b"+PO"); // partial
        drop(sock);
    });
    started_rx.recv().unwrap();
    let mut c = RespClient::connect("127.0.0.1", port).unwrap();
    let err = c.request(&[b"PING".to_vec()]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}
