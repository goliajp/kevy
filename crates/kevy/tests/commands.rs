//! v0.2 detection: the core string/key command surface over real RESP, against
//! a live keyspace shared across the commands of one connection.

use std::io::{Read, Write};

/// Run a sequence of (request, expected-reply) pairs over one connection that
/// shares a single keyspace, asserting each reply.
fn exchange(steps: &[(&[u8], &[u8])]) {
    let listener = kevy_sys::tcp_listen([127, 0, 0, 1], 0, 16).unwrap();
    let port = listener.local_port().unwrap();

    let server = std::thread::spawn(move || {
        let conn = listener.accept().unwrap();
        let mut store = kevy::KeyspaceStore::new();
        kevy::handle_conn(&conn, &mut store).unwrap();
    });

    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    for (req, want) in steps {
        c.write_all(req).unwrap();
        let mut buf = vec![0u8; want.len()];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, want, "request {:?}", String::from_utf8_lossy(req));
    }
    drop(c);
    server.join().unwrap();
}

/// Build a RESP multi-bulk request from argv pieces.
fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

#[test]
fn string_lifecycle() {
    let r_set = req(&[b"SET", b"foo", b"bar"]);
    let r_get = req(&[b"GET", b"foo"]);
    let r_getmiss = req(&[b"GET", b"absent"]);
    let r_strlen = req(&[b"STRLEN", b"foo"]);
    let r_append = req(&[b"APPEND", b"foo", b"baz"]);
    let r_getafter = req(&[b"GET", b"foo"]);
    let r_type = req(&[b"TYPE", b"foo"]);
    let r_exists = req(&[b"EXISTS", b"foo", b"absent"]);
    let r_del = req(&[b"DEL", b"foo"]);
    let r_getgone = req(&[b"GET", b"foo"]);

    exchange(&[
        (&r_set, b"+OK\r\n"),
        (&r_get, b"$3\r\nbar\r\n"),
        (&r_getmiss, b"$-1\r\n"),
        (&r_strlen, b":3\r\n"),
        (&r_append, b":6\r\n"),
        (&r_getafter, b"$6\r\nbarbaz\r\n"),
        (&r_type, b"+string\r\n"),
        (&r_exists, b":1\r\n"),
        (&r_del, b":1\r\n"),
        (&r_getgone, b"$-1\r\n"),
    ]);
}

#[test]
fn counters() {
    let r_incr = req(&[b"INCR", b"n"]);
    let r_incrby = req(&[b"INCRBY", b"n", b"10"]);
    let r_decr = req(&[b"DECR", b"n"]);
    let r_decrby = req(&[b"DECRBY", b"n", b"4"]);
    let r_set = req(&[b"SET", b"s", b"abc"]);
    let r_bad = req(&[b"INCR", b"s"]);

    exchange(&[
        (&r_incr, b":1\r\n"),
        (&r_incrby, b":11\r\n"),
        (&r_decr, b":10\r\n"),
        (&r_decrby, b":6\r\n"),
        (&r_set, b"+OK\r\n"),
        (&r_bad, b"-ERR value is not an integer or out of range\r\n"),
    ]);
}

#[test]
fn expiry_commands() {
    let r_set = req(&[b"SET", b"k", b"v"]);
    let r_ttl_none = req(&[b"TTL", b"k"]);
    let r_expire = req(&[b"EXPIRE", b"k", b"100"]);
    let r_ttl = req(&[b"TTL", b"k"]);
    let r_persist = req(&[b"PERSIST", b"k"]);
    let r_ttl_after = req(&[b"TTL", b"k"]);
    let r_ttl_missing = req(&[b"TTL", b"nope"]);

    exchange(&[
        (&r_set, b"+OK\r\n"),
        (&r_ttl_none, b":-1\r\n"),
        (&r_expire, b":1\r\n"),
        (&r_ttl, b":100\r\n"),
        (&r_persist, b":1\r\n"),
        (&r_ttl_after, b":-1\r\n"),
        (&r_ttl_missing, b":-2\r\n"),
    ]);
}

#[test]
fn set_options_and_flush() {
    let r_setnx_ok = req(&[b"SET", b"k", b"v1", b"NX"]);
    let r_setnx_no = req(&[b"SET", b"k", b"v2", b"NX"]);
    let r_get = req(&[b"GET", b"k"]);
    let r_setxx_ok = req(&[b"SET", b"k", b"v3", b"XX"]);
    let r_setex = req(&[b"SET", b"k", b"v4", b"EX", b"50"]);
    let r_ttl = req(&[b"TTL", b"k"]);
    let r_dbsize = req(&[b"DBSIZE"]);
    let r_flush = req(&[b"FLUSHDB"]);
    let r_dbsize0 = req(&[b"DBSIZE"]);

    exchange(&[
        (&r_setnx_ok, b"+OK\r\n"),
        (&r_setnx_no, b"$-1\r\n"),
        (&r_get, b"$2\r\nv1\r\n"),
        (&r_setxx_ok, b"+OK\r\n"),
        (&r_setex, b"+OK\r\n"),
        (&r_ttl, b":50\r\n"),
        (&r_dbsize, b":1\r\n"),
        (&r_flush, b"+OK\r\n"),
        (&r_dbsize0, b":0\r\n"),
    ]);
}
