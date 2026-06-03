//! Mock-RESP round-trip for `Transaction` — drives the MULTI / QUEUED /
//! EXEC / DISCARD wire shapes against a tiny scripted server.

use kevy_client::Connection;
use kevy_resp::Reply;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Spin a one-shot mock that, after accepting one connection, scripts
/// `(expect_in_at_least, reply_bytes)` rounds in order. Closes after the
/// last reply.
fn mock_server(rounds: Vec<(usize, &'static [u8])>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (started_tx, started_rx) = mpsc::channel();
    thread::spawn(move || {
        started_tx.send(()).unwrap();
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = vec![0u8; 4096];
        for (need, reply) in rounds {
            let mut total = 0;
            while total < need {
                match sock.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(_) => break,
                }
            }
            let _ = sock.write_all(reply);
        }
        thread::sleep(Duration::from_millis(50));
    });
    started_rx.recv().unwrap();
    port
}

#[test]
fn multi_queue_exec_returns_array() {
    // *1\r\n$5\r\nMULTI\r\n             → +OK
    // *3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n → +QUEUED
    // *2\r\n$4\r\nINCR\r\n$1\r\na\r\n   → +QUEUED
    // *1\r\n$4\r\nEXEC\r\n              → *2\r\n+OK\r\n:2\r\n
    let port = mock_server(vec![
        (14, b"+OK\r\n"),
        (29, b"+QUEUED\r\n"),
        (23, b"+QUEUED\r\n"),
        (13, b"*2\r\n+OK\r\n:2\r\n"),
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.queue(&[b"SET", b"a", b"1"]).unwrap();
    txn.queue(&[b"INCR", b"a"]).unwrap();
    let replies = txn.exec().unwrap();
    assert_eq!(replies.len(), 2);
    assert!(matches!(&replies[0], Reply::Simple(s) if s == b"OK"));
    assert!(matches!(&replies[1], Reply::Int(2)));
}

#[test]
fn multi_discard_clears_queue() {
    let port = mock_server(vec![
        (14, b"+OK\r\n"),    // MULTI
        (29, b"+QUEUED\r\n"), // SET a 1
        (8, b"+OK\r\n"),     // DISCARD
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.queue(&[b"SET", b"a", b"1"]).unwrap();
    txn.discard().unwrap();
}

#[test]
fn multi_drop_sends_implicit_discard() {
    let port = mock_server(vec![
        (14, b"+OK\r\n"),    // MULTI
        (29, b"+QUEUED\r\n"), // SET a 1
        (8, b"+OK\r\n"),     // DISCARD via Drop
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    {
        let mut txn = conn.multi().unwrap();
        txn.queue(&[b"SET", b"a", b"1"]).unwrap();
        // No exec/discard — Drop fires DISCARD on the wire.
    }
}
