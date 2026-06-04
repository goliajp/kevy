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

// ─── v1.5.0: typed builders + WATCH ───────────────────────────────────────

#[test]
fn typed_builders_chain_and_exec() {
    // MULTI → +OK; SET a 1 → +QUEUED; INCR c → +QUEUED; DEL b → +QUEUED;
    // EXEC → *3\r\n+OK\r\n:5\r\n:1\r\n
    let port = mock_server(vec![
        (14, b"+OK\r\n"),
        (29, b"+QUEUED\r\n"),
        (23, b"+QUEUED\r\n"),
        (22, b"+QUEUED\r\n"),
        (13, b"*3\r\n+OK\r\n:5\r\n:1\r\n"),
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.set(b"a", b"1")
        .unwrap()
        .incr(b"c")
        .unwrap()
        .del(&[b"b"])
        .unwrap();
    let replies = txn.exec().unwrap();
    assert_eq!(replies.len(), 3);
    assert!(matches!(&replies[0], Reply::Simple(s) if s == b"OK"));
    assert!(matches!(&replies[1], Reply::Int(5)));
    assert!(matches!(&replies[2], Reply::Int(1)));
}

#[test]
fn watch_then_multi_exec_success() {
    // WATCH x → +OK; MULTI → +OK; INCR x → +QUEUED; EXEC → *1\r\n:7\r\n
    let port = mock_server(vec![
        (20, b"+OK\r\n"),     // WATCH x
        (14, b"+OK\r\n"),     // MULTI
        (23, b"+QUEUED\r\n"), // INCR x
        (13, b"*1\r\n:7\r\n"), // EXEC
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    conn.watch(&[b"x"]).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.incr(b"x").unwrap();
    let replies = txn.exec_watched().unwrap().expect("not aborted");
    assert_eq!(replies.len(), 1);
    assert!(matches!(&replies[0], Reply::Int(7)));
}

#[test]
fn watch_then_exec_aborted_returns_none() {
    // WATCH x → +OK; MULTI → +OK; INCR x → +QUEUED; EXEC → $-1\r\n (Nil)
    let port = mock_server(vec![
        (20, b"+OK\r\n"),
        (14, b"+OK\r\n"),
        (23, b"+QUEUED\r\n"),
        (13, b"$-1\r\n"), // RESP2 null bulk — Reply::Nil
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    conn.watch(&[b"x"]).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.incr(b"x").unwrap();
    assert!(txn.exec_watched().unwrap().is_none());
}

#[test]
fn unwatch_sends_off_the_wire() {
    let port = mock_server(vec![(13, b"+OK\r\n")]); // UNWATCH
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    conn.unwatch().unwrap();
}

// ─── v1.7.0: typed exec cursor (TransactionReplies) ───────────────────────

#[test]
fn exec_typed_reads_mixed_replies_in_order() {
    // MULTI → SET a 1 (+OK) / INCR c (:5) / GET a ($1 1) / MGET b c (*2 $-1 $1 5)
    let port = mock_server(vec![
        (14, b"+OK\r\n"),                                     // MULTI
        (29, b"+QUEUED\r\n"),                                 // SET a 1
        (23, b"+QUEUED\r\n"),                                 // INCR c
        (22, b"+QUEUED\r\n"),                                 // GET a
        (28, b"+QUEUED\r\n"),                                 // MGET b c
        (
            13,
            b"*4\r\n+OK\r\n:5\r\n$1\r\n1\r\n*2\r\n$-1\r\n$1\r\n5\r\n",
        ), // EXEC
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.set(b"a", b"1")
        .unwrap()
        .incr(b"c")
        .unwrap()
        .get(b"a")
        .unwrap()
        .mget(&[b"b", b"c"])
        .unwrap();
    let mut r = txn.exec_typed().unwrap();
    assert_eq!(r.remaining(), 4);
    r.next_ok().unwrap();
    assert_eq!(r.next_int().unwrap(), 5);
    assert_eq!(r.next_bulk().unwrap(), Some(b"1".to_vec()));
    assert_eq!(
        r.next_array_of_bulks().unwrap(),
        vec![None, Some(b"5".to_vec())]
    );
    r.expect_empty().unwrap();
}

#[test]
fn exec_typed_type_mismatch_surfaces_invalid_data() {
    let port = mock_server(vec![
        (14, b"+OK\r\n"),       // MULTI
        (23, b"+QUEUED\r\n"),   // INCR c
        (13, b"*1\r\n:5\r\n"),  // EXEC
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.incr(b"c").unwrap();
    let mut r = txn.exec_typed().unwrap();
    // Ask for a Bulk when the next reply is actually Int → InvalidData.
    let err = r.next_bulk().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("expected Bulk"),
        "msg = {err}"
    );
}

#[test]
fn exec_typed_aborted_by_watch_errors() {
    let port = mock_server(vec![
        (20, b"+OK\r\n"),     // WATCH x
        (14, b"+OK\r\n"),     // MULTI
        (23, b"+QUEUED\r\n"), // INCR x
        (13, b"$-1\r\n"),     // EXEC → Nil (aborted)
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    conn.watch(&[b"x"]).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.incr(b"x").unwrap();
    let err = txn.exec_typed().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("WATCH"), "msg = {err}");
}

#[test]
fn exec_watched_typed_returns_none_on_abort() {
    let port = mock_server(vec![
        (20, b"+OK\r\n"),
        (14, b"+OK\r\n"),
        (23, b"+QUEUED\r\n"),
        (13, b"$-1\r\n"),
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    conn.watch(&[b"x"]).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.incr(b"x").unwrap();
    assert!(txn.exec_watched_typed().unwrap().is_none());
}

#[test]
fn expect_empty_errors_when_replies_left_unconsumed() {
    let port = mock_server(vec![
        (14, b"+OK\r\n"),
        (23, b"+QUEUED\r\n"),
        (23, b"+QUEUED\r\n"),
        (13, b"*2\r\n:1\r\n:2\r\n"),
    ]);
    let mut conn = Connection::open(&format!("kevy://127.0.0.1:{port}")).unwrap();
    let mut txn = conn.multi().unwrap();
    txn.incr(b"a").unwrap().incr(b"b").unwrap();
    let mut r = txn.exec_typed().unwrap();
    assert_eq!(r.next_int().unwrap(), 1);
    let err = r.expect_empty().unwrap_err();
    assert!(err.to_string().contains("1 un-consumed"), "msg = {err}");
}

#[test]
fn watch_on_embedded_returns_unsupported() {
    // Embedded backend has no MULTI dispatcher; WATCH is a no-op there
    // and must surface as Unsupported so callers don't silently miss
    // the optimistic-concurrency guarantee.
    let mut conn = Connection::open("mem://watch-embed-probe").unwrap();
    let err = conn.watch(&[b"x"]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    let err = conn.unwatch().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}
