//! Integration test (T4.21): wire round-trip under the `smol` runtime
//! feature. Same scenarios as `tokio_basic.rs` adapted for smol's
//! `block_on` + `spawn`. Only compiled when `smol` is enabled.

#![cfg(feature = "smol")]

use std::io;

use kevy_client_async::AsyncConnection;
use kevy_resp::Reply;
use smol::io::{AsyncReadExt, AsyncWriteExt};
use smol::net::{TcpListener, TcpStream};

async fn spawn_replier_seq(steps: Vec<(Vec<u8>, Vec<u8>)>) -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    smol::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        for (incoming, outgoing) in steps {
            let mut buf = vec![0u8; incoming.len()];
            sock.read_exact(&mut buf).await.expect("read");
            assert_eq!(buf, incoming, "client wire mismatch");
            sock.write_all(&outgoing).await.expect("write");
        }
        sock.close().await.ok();
    })
    .detach();
    Ok(port)
}

async fn spawn_replier(
    incoming_expected: Vec<u8>,
    outgoing: Vec<u8>,
) -> io::Result<u16> {
    spawn_replier_seq(vec![(incoming_expected, outgoing)]).await
}

#[test]
fn ping_round_trip() {
    smol::block_on(async {
        let port = spawn_replier(b"*1\r\n$4\r\nPING\r\n".to_vec(), b"+PONG\r\n".to_vec())
            .await
            .unwrap();
        let url = format!("tcp://127.0.0.1:{port}");
        let mut conn = AsyncConnection::open(&url).await.unwrap();
        conn.ping().await.unwrap();
    });
}

#[test]
fn set_then_get() {
    smol::block_on(async {
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
    });
}

#[test]
fn pipeline_one_round_trip() {
    smol::block_on(async {
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
    });
}

#[test]
fn from_transport_accepts_smol_tcpstream() {
    smol::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        smol::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 32];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"+PONG\r\n").await.unwrap();
        })
        .detach();
        let s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut conn = AsyncConnection::from_transport(s);
        conn.ping().await.unwrap();
    });
}
