//! `kevys://` under tokio: a stand-in server runs the Noise responder and
//! answers sealed replies, one of them larger than a Noise message.

#![cfg(feature = "tokio")]

use kevy_client_async::AsyncConnection;
use kevy_client_async::subscriber::AsyncSubscriber;
use kevy_noise::{Frames, Keypair, Responder, frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PROLOGUE: &[u8] = b"kevy-client\x001";

async fn next_message(sock: &mut TcpStream, frames: &mut Frames) -> Option<Vec<u8>> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(m) = frames.next() {
            return Some(m);
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        frames.push(&chunk[..n]);
    }
}

/// Handshake each connection, then answer the n-th request with
/// `replies[n]`, sealed. Returns the port and the server's public key.
async fn fake_server(replies: Vec<Vec<u8>>) -> (u16, [u8; 32]) {
    let key = Keypair::from_secret([1; 32]);
    let public = key.public();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let mut frames = Frames::default();
        let m1 = next_message(&mut s, &mut frames).await.unwrap();
        let (_, r) = Responder::accept(&key, Keypair::from_secret([2; 32]), PROLOGUE, &m1).unwrap();
        let (m2, t) = r.finish(b"").unwrap();
        s.write_all(&frame(&m2).unwrap()).await.unwrap();
        let (mut tx, mut rx) = t.split();
        for reply in replies {
            let req = next_message(&mut s, &mut frames).await.unwrap();
            assert!(rx.open(&req).unwrap().starts_with(b"*"), "a RESP request arrives");
            for chunk in reply.chunks(60_000) {
                s.write_all(&frame(&tx.seal(chunk).unwrap()).unwrap()).await.unwrap();
            }
        }
    });
    (port, public)
}

fn hex(k: &[u8; 32]) -> String {
    k.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn a_kevys_connection_pings_and_reads_a_large_value() {
    let big = vec![b'x'; 150_000];
    let mut bulk = format!("${}\r\n", big.len()).into_bytes();
    bulk.extend_from_slice(&big);
    bulk.extend_from_slice(b"\r\n");
    let (port, key) = fake_server(vec![b"+PONG\r\n".to_vec(), bulk]).await;
    let url = format!("kevys://127.0.0.1:{port}?server_key={}", hex(&key));
    let mut c = AsyncConnection::connect_secure_url(&url).await.unwrap();
    c.ping().await.unwrap();
    assert_eq!(c.get(b"k").await.unwrap(), Some(big));
}

#[tokio::test]
async fn a_kevys_subscriber_gets_its_ack() {
    let ack = b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n".to_vec();
    let (port, key) = fake_server(vec![ack]).await;
    let url = format!("kevys://127.0.0.1:{port}?server_key={}", hex(&key));
    let mut s = AsyncSubscriber::connect_secure_url(&url).await.unwrap();
    s.subscribe(&[b"news"]).await.unwrap();
}

#[tokio::test]
async fn a_wrong_server_key_is_refused() {
    let (port, _) = fake_server(Vec::new()).await;
    let url = format!("kevys://127.0.0.1:{port}?server_key={}", "ab".repeat(32));
    let refused = AsyncConnection::connect_secure_url(&url).await;
    assert!(refused.is_err());
    let _ = TcpStream::connect(("127.0.0.1", port)).await;
}
