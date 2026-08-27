//! Integration test: end-to-end wire round-trip for the tokio
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
    let mut conn = AsyncConnection::connect(&url).await.unwrap();
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
    let mut conn = AsyncConnection::connect(&url).await.unwrap();
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
    let mut conn = AsyncConnection::connect(&url).await.unwrap();
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
    let mut conn = AsyncConnection::connect(&url).await.unwrap();
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

// ---------------------------------------------------------------------
// Command families.
//
// The dead-path atlas put this crate at 70.5% never-executed, the highest
// rate in the workspace outside the language doors. The named symbols were
// not exotic: `cmd_list::list_push`, `cmd_list::list_pop`,
// `cmd_set::set_multi`, `cmd_set::set_combine`, `reply::array_to_bulks`,
// `reply::unexpected` — the ordinary command surface, with five tests above
// it that only ever exercised PING, SET, GET and a pipeline.
//
// Every request byte string below is the exact frame the helper builds
// (`request_borrowed(&[verb, key, …])`), read off the implementation rather
// than guessed: `list_pop` always sends three arguments including the count,
// `set_combine` sends the verb followed by the keys and no key count.

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

async fn one(step: (Vec<u8>, Vec<u8>)) -> AsyncConnection {
    let port = spawn_replier_seq(vec![step]).await.unwrap();
    AsyncConnection::connect(&format!("tcp://127.0.0.1:{port}")).await.unwrap()
}

#[tokio::test]
async fn list_push_sends_every_value_and_returns_the_length() {
    let mut c = one((
        req(&[b"LPUSH", b"l", b"a", b"b"]),
        b":2\r\n".to_vec(),
    ))
    .await;
    assert_eq!(c.lpush(b"l", &[b"a", b"b"]).await.unwrap(), 2);

    let mut c = one((req(&[b"RPUSH", b"l", b"z"]), b":7\r\n".to_vec())).await;
    assert_eq!(c.rpush(b"l", &[b"z"]).await.unwrap(), 7);
}

#[tokio::test]
async fn list_pop_reads_an_array_a_bulk_and_a_nil() {
    // The count is always on the wire, even at 1.
    let mut c = one((
        req(&[b"LPOP", b"l", b"2"]),
        b"*2\r\n$1\r\na\r\n$1\r\nb\r\n".to_vec(),
    ))
    .await;
    assert_eq!(c.lpop(b"l", 2).await.unwrap(), vec![b"a".to_vec(), b"b".to_vec()]);

    // A single-element pop may come back as a bare bulk rather than a
    // one-element array; both mean one element.
    let mut c = one((req(&[b"RPOP", b"l", b"1"]), b"$1\r\nz\r\n".to_vec())).await;
    assert_eq!(c.rpop(b"l", 1).await.unwrap(), vec![b"z".to_vec()]);

    // Nil is an empty result, not an error.
    let mut c = one((req(&[b"LPOP", b"gone", b"1"]), b"$-1\r\n".to_vec())).await;
    assert!(c.lpop(b"gone", 1).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_server_error_becomes_an_io_error_carrying_its_text() {
    let mut c = one((
        req(&[b"LPOP", b"l", b"1"]),
        b"-WRONGTYPE not a list\r\n".to_vec(),
    ))
    .await;
    let e = c.lpop(b"l", 1).await.unwrap_err();
    assert!(
        e.to_string().contains("WRONGTYPE"),
        "the server's own words must survive: {e}"
    );
}

#[tokio::test]
async fn a_reply_of_the_wrong_shape_is_refused_not_coerced() {
    // `:1` is not a list. `reply::unexpected` is the branch that says so
    // rather than inventing an element.
    let mut c = one((req(&[b"LPOP", b"l", b"1"]), b":1\r\n".to_vec())).await;
    assert!(c.lpop(b"l", 1).await.is_err(), "an integer is not a popped list");
}

#[tokio::test]
async fn set_multi_and_set_combine_send_what_they_are_given() {
    let mut c = one((req(&[b"SADD", b"s", b"a", b"b"]), b":2\r\n".to_vec())).await;
    assert_eq!(c.sadd(b"s", &[b"a", b"b"]).await.unwrap(), 2);

    let mut c = one((req(&[b"SREM", b"s", b"a"]), b":1\r\n".to_vec())).await;
    assert_eq!(c.srem(b"s", &[b"a"]).await.unwrap(), 1);

    // set_combine takes keys only — no key count on the wire.
    let mut c = one((
        req(&[b"SINTER", b"s1", b"s2"]),
        b"*1\r\n$1\r\nx\r\n".to_vec(),
    ))
    .await;
    assert_eq!(c.sinter(&[b"s1", b"s2"]).await.unwrap(), vec![b"x".to_vec()]);

    let mut c = one((req(&[b"SDIFF", b"s1", b"s2"]), b"*0\r\n".to_vec())).await;
    assert!(c.sdiff(&[b"s1", b"s2"]).await.unwrap().is_empty());
}

#[tokio::test]
async fn array_to_bulks_carries_a_flat_field_value_run() {
    // HGETALL is the flat [f, v, f, v] shape; the helper must not pair or
    // reorder it.
    let mut c = one((
        req(&[b"HGETALL", b"h"]),
        b"*4\r\n$2\r\nf1\r\n$2\r\nv1\r\n$2\r\nf2\r\n$2\r\nv2\r\n".to_vec(),
    ))
    .await;
    assert_eq!(
        c.hgetall(b"h").await.unwrap(),
        vec![b"f1".to_vec(), b"v1".to_vec(), b"f2".to_vec(), b"v2".to_vec()]
    );
}

#[tokio::test]
async fn string_verbs_send_their_redis_spelling() {
    // `expire` takes a Duration and sends PEXPIRE with milliseconds — the
    // conversion is the part worth pinning, since a caller writing
    // `Duration::from_secs(1)` must not produce `EXPIRE 1000`.
    let mut c = one((req(&[b"PEXPIRE", b"k", b"1500"]), b":1\r\n".to_vec())).await;
    assert!(c.expire(b"k", std::time::Duration::from_millis(1500)).await.unwrap());

    let mut c = one((req(&[b"PERSIST", b"k"]), b":0\r\n".to_vec())).await;
    assert!(!c.persist(b"k").await.unwrap(), ":0 is false, not an error");

    let mut c = one((req(&[b"INCRBY", b"n", b"-3"]), b":7\r\n".to_vec())).await;
    assert_eq!(c.incr_by(b"n", -3).await.unwrap(), 7, "a negative delta is still INCRBY");

    let mut c = one((req(&[b"DEL", b"a", b"b"]), b":2\r\n".to_vec())).await;
    assert_eq!(c.del(&[b"a", b"b"]).await.unwrap(), 2);

    let mut c = one((req(&[b"EXISTS", b"a"]), b":0\r\n".to_vec())).await;
    assert_eq!(c.exists(&[b"a"]).await.unwrap(), 0);

    let mut c = one((req(&[b"TYPE", b"k"]), b"+string\r\n".to_vec())).await;
    assert_eq!(c.type_of(b"k").await.unwrap(), "string");

    let mut c = one((req(&[b"PTTL", b"k"]), b":-1\r\n".to_vec())).await;
    assert_eq!(c.ttl_ms(b"k").await.unwrap(), -1, "-1 means no TTL, and survives");
}

#[tokio::test]
async fn hash_verbs_flatten_pairs_in_order() {
    let mut c = one((
        req(&[b"HSET", b"h", b"f1", b"v1", b"f2", b"v2"]),
        b":2\r\n".to_vec(),
    ))
    .await;
    assert_eq!(c.hset(b"h", &[(&b"f1"[..], &b"v1"[..]), (&b"f2"[..], &b"v2"[..])]).await.unwrap(), 2);

    let mut c = one((req(&[b"HGET", b"h", b"f1"]), b"$2\r\nv1\r\n".to_vec())).await;
    assert_eq!(c.hget(b"h", b"f1").await.unwrap().as_deref(), Some(&b"v1"[..]));

    // A missing field is None, not an empty vec and not an error.
    let mut c = one((req(&[b"HGET", b"h", b"nope"]), b"$-1\r\n".to_vec())).await;
    assert_eq!(c.hget(b"h", b"nope").await.unwrap(), None);

    let mut c = one((req(&[b"HDEL", b"h", b"f1"]), b":1\r\n".to_vec())).await;
    assert_eq!(c.hdel(b"h", &[b"f1"]).await.unwrap(), 1);

    let mut c = one((req(&[b"HKEYS", b"h"]), b"*1\r\n$2\r\nf2\r\n".to_vec())).await;
    assert_eq!(c.hkeys(b"h").await.unwrap(), vec![b"f2".to_vec()]);

    let mut c = one((req(&[b"HLEN", b"h"]), b":1\r\n".to_vec())).await;
    assert_eq!(c.hlen(b"h").await.unwrap(), 1);
}

#[tokio::test]
async fn zset_verbs_interleave_score_then_member() {
    // ZADD's wire order is score-first per pair, and the score is a
    // formatted float — `1` not `1.0` for a whole number, because that is
    // what `f64::to_string` produces and what the server parses.
    let mut c = one((
        req(&[b"ZADD", b"z", b"1", b"one", b"2.5", b"two"]),
        b":2\r\n".to_vec(),
    ))
    .await;
    assert_eq!(
        c.zadd(b"z", &[(1.0, &b"one"[..]), (2.5, &b"two"[..])]).await.unwrap(),
        2
    );

    let mut c = one((req(&[b"ZSCORE", b"z", b"one"]), b"$1\r\n1\r\n".to_vec())).await;
    assert_eq!(c.zscore(b"z", b"one").await.unwrap(), Some(1.0));

    // A member that is not there scores None rather than 0.
    let mut c = one((req(&[b"ZSCORE", b"z", b"gone"]), b"$-1\r\n".to_vec())).await;
    assert_eq!(c.zscore(b"z", b"gone").await.unwrap(), None);

    let mut c = one((req(&[b"ZREM", b"z", b"one"]), b":1\r\n".to_vec())).await;
    assert_eq!(c.zrem(b"z", &[b"one"]).await.unwrap(), 1);

    let mut c = one((req(&[b"ZCARD", b"z"]), b":1\r\n".to_vec())).await;
    assert_eq!(c.zcard(b"z").await.unwrap(), 1);
}
