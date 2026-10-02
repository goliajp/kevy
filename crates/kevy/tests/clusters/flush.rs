//! `FLUSHALL [ASYNC | SYNC]` on a sharded server: every shard empties, the
//! asynchronous form frees off the reactors, and other arguments are
//! refused before anything is flushed.

use std::io::Write;

use super::pipeline_order::{Server, read_reply, req};

#[test]
fn flushall_async_empties_every_shard_and_bad_arguments_flush_nothing() {
    let srv = Server::start();
    let mut c = srv.connect();
    let mut call = |parts: &[&[u8]]| {
        c.write_all(&req(parts)).unwrap();
        read_reply(&mut c)
    };
    let fill = |call: &mut dyn FnMut(&[&[u8]]) -> Vec<u8>| {
        for i in 0..64 {
            let k = format!("k{i}");
            call(&[b"HSET", k.as_bytes(), b"f", b"v"]);
        }
    };
    fill(&mut call);
    assert_eq!(call(&[b"FLUSHALL", b"FOO"]), b"-ERR syntax error\r\n");
    assert_eq!(call(&[b"FLUSHALL", b"ASYNC", b"SYNC"]), b"-ERR syntax error\r\n");
    assert_eq!(call(&[b"DBSIZE"]), b":64\r\n");
    assert_eq!(call(&[b"FLUSHALL", b"async"]), b"+OK\r\n");
    assert_eq!(call(&[b"DBSIZE"]), b":0\r\n");
    fill(&mut call);
    assert_eq!(call(&[b"FLUSHDB", b"SYNC"]), b"+OK\r\n");
    assert_eq!(call(&[b"DBSIZE"]), b":0\r\n");
}
