//! Integration test (T4.20): end-to-end wire round-trip for the tokio
//! runtime feature. Spawns a minimum RESP "server" inside the test
//! process, runs the async client against it, and checks both sides
//! of the byte stream.
//!
//! Only compiled when the `tokio` feature is enabled. Smol +
//! async-std equivalents live in `smol_basic.rs` / `async_std_basic.rs`.

#![cfg(feature = "tokio")]

use std::io;

use kevy_client_async::AsyncConnection;
use kevy_resp::Reply;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Spawn a fake RESP server that handles a sequence of
/// (read-this-many-bytes, write-this) interactions. Use one tuple per
/// request the client will send so the test does not deadlock against
/// a sequential client waiting on a reply before sending the next
/// command.
async fn spawn_replier_seq(steps: Vec<(Vec<u8>, Vec<u8>)>) -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        for (incoming, outgoing) in steps {
            let mut buf = vec![0u8; incoming.len()];
            sock.read_exact(&mut buf).await.expect("read");
            assert_eq!(buf, incoming, "client wire mismatch");
            sock.write_all(&outgoing).await.expect("write");
        }
        sock.shutdown().await.ok();
    });
    Ok(port)
}

/// Shorthand for a one-shot interaction (single read followed by
/// single write). Used by tests that send exactly one command + one
/// pipeline batch.
async fn spawn_replier(
    incoming_expected: Vec<u8>,
    outgoing: Vec<u8>,
) -> io::Result<u16> {
    spawn_replier_seq(vec![(incoming_expected, outgoing)]).await
}

#[tokio::test]
async fn ping_round_trip() {
    let port = spawn_replier(b"*1\r\n$4\r\nPING\r\n".to_vec(), b"+PONG\r\n".to_vec())
        .await
        .unwrap();
    let url = format!("tcp://127.0.0.1:{port}");
    let mut conn = AsyncConnection::open(&url).await.unwrap();
    conn.ping().await.unwrap();
}

#[tokio::test]
async fn set_then_get() {
    // Two sequential requests: client waits for SET's +OK before
    // sending GET, so the fake server must read+reply twice.
    let port = spawn_replier_seq(vec![
        (
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n".to_vec(),
            b"+OK\r\n".to_vec(),
        ),
        (
            b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n".to_vec(),
            b"$1\r\nv\r\n".to_vec(),
        ),
    ])
    .await
    .unwrap();
    let url = format!("tcp://127.0.0.1:{port}");
    let mut conn = AsyncConnection::open(&url).await.unwrap();
    conn.set(b"k", b"v").await.unwrap();
    let v = conn.get(b"k").await.unwrap();
    assert_eq!(v.as_deref(), Some(&b"v"[..]));
}

#[tokio::test]
async fn pipeline_one_round_trip() {
    // Three commands in one batched write, three replies in one read.
    let port = spawn_replier(
        b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n\
          *2\r\n$3\r\nGET\r\n$1\r\nk\r\n\
          *2\r\n$4\r\nINCR\r\n$3\r\ncnt\r\n"
            .to_vec(),
        b"+OK\r\n$1\r\nv\r\n:1\r\n".to_vec(),
    )
    .await
    .unwrap();
    let url = format!("tcp://127.0.0.1:{port}");
    let mut conn = AsyncConnection::open(&url).await.unwrap();
    let replies = conn
        .pipeline()
        .set(b"k", b"v")
        .get(b"k")
        .incr(b"cnt")
        .run(&mut conn)
        .await
        .unwrap();
    assert_eq!(replies.len(), 3);
    assert!(matches!(replies[0], Reply::Simple(ref s) if s == b"OK"));
    assert!(matches!(replies[1], Reply::Bulk(ref v) if v == b"v"));
    assert!(matches!(replies[2], Reply::Int(1)));
}

#[tokio::test]
async fn server_close_yields_unexpected_eof() {
    // No reply at all — server closes after reading the command.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 32];
        let _ = sock.read(&mut buf).await;
        // Drop = close.
    });
    let url = format!("tcp://127.0.0.1:{port}");
    let mut conn = AsyncConnection::open(&url).await.unwrap();
    let err = conn.ping().await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn connect_works_with_real_tcpstream_typeshape() {
    // Sanity: the type alias resolves to tokio::net::TcpStream so a
    // user-supplied TcpStream can be wrapped via from_transport.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        // Reply +PONG to whatever lands.
        let mut buf = [0u8; 32];
        let _ = sock.read(&mut buf).await;
        sock.write_all(b"+PONG\r\n").await.unwrap();
    });
    let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut conn = AsyncConnection::from_transport(s);
    conn.ping().await.unwrap();
}
