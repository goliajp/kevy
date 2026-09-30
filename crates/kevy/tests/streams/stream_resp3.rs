//! The stream replies under RESP3, byte for byte as valkey 9.1.2 gives
//! them: `XREAD` and `XREADGROUP` answer a map of stream to entries, and
//! a missing reply, a deleted entry's fields included, is `_`. Four
//! shards, so the two-stream reads gather from more than one.

use std::io::{Read, Write};

use super::stream::{Server, req};

/// `(command, valkey's RESP3 reply)`, in order, after `HELLO 3`.
#[rustfmt::skip]
const CASES: &[(&str, &str)] = &[
    ("XADD s 1-0 f v", "$3\r\n1-0\r\n"),
    ("XADD s 2-0 f v", "$3\r\n2-0\r\n"),
    ("XADD t 1-0 g w", "$3\r\n1-0\r\n"),
    ("XGROUP CREATE s g 0", "+OK\r\n"),
    ("XGROUP CREATE t g 0", "+OK\r\n"),
    ("XREAD STREAMS s 0", "%1\r\n$1\r\ns\r\n*2\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n"),
    ("XREAD STREAMS s t 0 0", "%2\r\n$1\r\ns\r\n*2\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n$1\r\nt\r\n*1\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\ng\r\n$1\r\nw\r\n"),
    ("XREAD STREAMS s t 9 9", "_\r\n"),
    ("XREAD STREAMS s t + +", "%2\r\n$1\r\ns\r\n*1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n$1\r\nt\r\n*1\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\ng\r\n$1\r\nw\r\n"),
    ("XREADGROUP GROUP g a COUNT 1 STREAMS s >", "%1\r\n$1\r\ns\r\n*1\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n"),
    ("XDEL s 1-0", ":1\r\n"),
    ("XREADGROUP GROUP g a STREAMS s 0", "%1\r\n$1\r\ns\r\n*1\r\n*2\r\n$3\r\n1-0\r\n_\r\n"),
    ("XREADGROUP GROUP g b STREAMS s 0", "%1\r\n$1\r\ns\r\n*0\r\n"),
    ("XREADGROUP GROUP g a STREAMS s t > >", "%2\r\n$1\r\ns\r\n*1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n$1\r\nt\r\n*1\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\ng\r\n$1\r\nw\r\n"),
    ("XREADGROUP GROUP g a STREAMS s t 0 0", "%2\r\n$1\r\ns\r\n*2\r\n*2\r\n$3\r\n1-0\r\n_\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n$1\r\nt\r\n*1\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\ng\r\n$1\r\nw\r\n"),
    ("XREADGROUP GROUP g a STREAMS s >", "_\r\n"),
    ("XRANGE s - + COUNT 0", "_\r\n"),
    ("XRANGE s - +", "*1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n"),
    ("XREVRANGE s + -", "*1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n"),
    ("XADD s NOMKSTREAM 3-0 f v", "$3\r\n3-0\r\n"),
    ("XADD nokey NOMKSTREAM 1-0 f v", "_\r\n"),
    ("XPENDING s g", "*4\r\n:2\r\n$3\r\n1-0\r\n$3\r\n2-0\r\n*1\r\n*2\r\n$1\r\na\r\n$1\r\n2\r\n"),
    ("XPENDING t g IDLE 99999999 - + 10", "*0\r\n"),
    ("XCLAIM s g b 0 2-0", "*1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n"),
    ("XCLAIM s g b 0 1-0", "*0\r\n"),
    ("XAUTOCLAIM s g c 0 0", "*3\r\n$3\r\n0-0\r\n*1\r\n*2\r\n$3\r\n2-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n*0\r\n"),
    ("XAUTOCLAIM s g c 0 0 JUSTID", "*3\r\n$3\r\n0-0\r\n*1\r\n$3\r\n2-0\r\n*0\r\n"),
    ("XREAD BLOCK 30 STREAMS s $", "_\r\n"),
    ("XREADGROUP GROUP g a BLOCK 30 STREAMS s >", "%1\r\n$1\r\ns\r\n*1\r\n*2\r\n$3\r\n3-0\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n"),
    ("XREAD BLOCK 30 STREAMS s t $ $", "_\r\n"),
    ("XINFO GROUPS s", "*1\r\n%6\r\n$4\r\nname\r\n$1\r\ng\r\n$9\r\nconsumers\r\n:3\r\n$7\r\npending\r\n:2\r\n$17\r\nlast-delivered-id\r\n$3\r\n3-0\r\n$12\r\nentries-read\r\n:3\r\n$3\r\nlag\r\n:0\r\n"),
    ("XINFO STREAM t", "%10\r\n$6\r\nlength\r\n:1\r\n$15\r\nradix-tree-keys\r\n:1\r\n$16\r\nradix-tree-nodes\r\n:2\r\n$17\r\nlast-generated-id\r\n$3\r\n1-0\r\n$20\r\nmax-deleted-entry-id\r\n$3\r\n0-0\r\n$13\r\nentries-added\r\n:1\r\n$23\r\nrecorded-first-entry-id\r\n$3\r\n1-0\r\n$6\r\ngroups\r\n:1\r\n$11\r\nfirst-entry\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\ng\r\n$1\r\nw\r\n$10\r\nlast-entry\r\n*2\r\n$3\r\n1-0\r\n*2\r\n$1\r\ng\r\n$1\r\nw\r\n"),
];

fn byte(s: &mut std::net::TcpStream) -> u8 {
    let mut b = [0u8; 1];
    s.read_exact(&mut b).unwrap();
    b[0]
}

/// The rest of a header line; its number, 0 when it has none.
fn line(s: &mut std::net::TcpStream, out: &mut Vec<u8>) -> i64 {
    let start = out.len();
    while !out[start..].ends_with(b"\r\n") {
        out.push(byte(s));
    }
    std::str::from_utf8(&out[start..out.len() - 2]).unwrap().parse().unwrap_or(0)
}

/// One whole RESP3 reply.
fn reply(s: &mut std::net::TcpStream, out: &mut Vec<u8>) {
    let kind = byte(s);
    out.push(kind);
    let n = line(s, out);
    let items = match kind {
        b'$' | b'=' | b'!' if n >= 0 => {
            let mut body = vec![0u8; n as usize + 2];
            s.read_exact(&mut body).unwrap();
            out.extend_from_slice(&body);
            0
        }
        b'*' | b'~' | b'>' => n.max(0),
        b'%' | b'|' => 2 * n,
        _ => 0,
    };
    for _ in 0..items {
        reply(s, out);
    }
}

fn call(c: &mut std::net::TcpStream, cmd: &str) -> String {
    let parts: Vec<&[u8]> = cmd.split(' ').map(str::as_bytes).collect();
    c.write_all(&req(&parts)).unwrap();
    let mut out = Vec::new();
    reply(c, &mut out);
    String::from_utf8(out).unwrap()
}

#[test]
fn stream_replies_under_resp3_are_valkeys() {
    let shard = |k: &[u8]| kevy_rt::shard_of_key(k, 4, kevy_persist::Routing::KevyHash);
    assert_ne!(shard(b"s"), shard(b"t"), "the two streams must be on different shards");
    let srv = Server::start(4);
    let mut c = srv.connect();
    assert!(call(&mut c, "HELLO 3").starts_with('%'));
    for (cmd, want) in CASES {
        assert_eq!(call(&mut c, cmd), *want, "{cmd}");
    }
}
