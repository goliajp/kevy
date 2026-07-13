//! Oracle gate: replay one deterministic command sequence against the
//! REAL server (`target/debug/kevy`) and the embedded full-surface
//! dispatcher, and compare the reply bytes.
//!
//! Non-deterministic replies (SCAN cursors, random members, cross-shard
//! ordering) compare by shape instead of bytes; TTL reads allow the
//! documented ±1 s / timing slack. Everything else must match exactly.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use kevy_embedded::{Config, Store};

const PORT: u16 = 6097; // dispatch-oracle port (6004 dev / 6096 taken elsewhere)

/// RAII kill for the oracle server — no zombie survives a panic.
struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("workspace root")
}

/// `target/debug/kevy`, building it first if absent.
fn server_binary() -> PathBuf {
    let root = workspace_root();
    let bin = root.join("target/debug/kevy");
    if !bin.exists() {
        let status = Command::new("cargo")
            .args(["build", "-p", "kevy"])
            .current_dir(&root)
            .status()
            .expect("spawn cargo build -p kevy");
        assert!(status.success(), "cargo build -p kevy failed");
    }
    bin
}

fn spawn_server(dir: &std::path::Path) -> ServerGuard {
    let child = Command::new(server_binary())
        .args(["--port", &PORT.to_string(), "--dir"])
        .arg(dir)
        .current_dir(dir) // never run a server from the repo root
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kevy server");
    ServerGuard(child)
}

fn connect_with_retry() -> TcpStream {
    for _ in 0..100 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", PORT)) {
            s.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
            return s;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("kevy server did not accept on port {PORT} within 10 s");
}

fn encode_req(argv: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", argv.len()).into_bytes();
    for a in argv {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Bytes consumed by one complete RESP reply at `b[0..]`, or `None`.
fn reply_len(b: &[u8]) -> Option<usize> {
    let nl = b.windows(2).position(|w| w == b"\r\n")?;
    let line = &b[..nl];
    match *line.first()? {
        b'+' | b'-' | b':' => Some(nl + 2),
        b'$' => {
            let n: i64 = std::str::from_utf8(&line[1..]).ok()?.parse().ok()?;
            if n < 0 {
                Some(nl + 2)
            } else {
                let end = nl + 2 + n as usize + 2;
                (b.len() >= end).then_some(end)
            }
        }
        b'*' => {
            let n: i64 = std::str::from_utf8(&line[1..]).ok()?.parse().ok()?;
            if n < 0 {
                return Some(nl + 2);
            }
            let mut pos = nl + 2;
            for _ in 0..n {
                pos += reply_len(&b[pos..])?;
            }
            Some(pos)
        }
        _ => None,
    }
}

fn server_reply(sock: &mut TcpStream, buf: &mut Vec<u8>, argv: &[&[u8]]) -> Vec<u8> {
    sock.write_all(&encode_req(argv)).expect("write request");
    loop {
        if let Some(n) = reply_len(buf) {
            return buf.drain(..n).collect();
        }
        let mut chunk = [0u8; 16384];
        let n = sock.read(&mut chunk).expect("read reply");
        assert!(n > 0, "server closed the connection");
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn embedded_reply(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
    out
}

/// How one command's two replies are held together.
enum Cmp {
    /// Byte-for-byte identical.
    Exact,
    /// Same RESP kind; for arrays, the same element count (ordering /
    /// randomness may differ).
    Shape,
    /// `[cursor, [keys…]]` envelope only — cursors and page sizes are
    /// implementation-defined.
    ScanShape,
    /// Integer replies within ±`0` (`:N` vs `:M`, |N−M| ≤ slack).
    IntSlack(i64),
}

fn first_elem_count(reply: &[u8]) -> i64 {
    let nl = reply.windows(2).position(|w| w == b"\r\n").expect("array header");
    std::str::from_utf8(&reply[1..nl]).expect("utf8").parse().expect("count")
}

fn assert_match(argv: &[&[u8]], cmp: &Cmp, srv: &[u8], emb: &[u8]) {
    let what = argv.iter().map(|a| String::from_utf8_lossy(a)).collect::<Vec<_>>().join(" ");
    match cmp {
        Cmp::Exact => assert_eq!(
            srv,
            emb,
            "reply mismatch for `{what}`: server {:?} vs embedded {:?}",
            String::from_utf8_lossy(srv),
            String::from_utf8_lossy(emb)
        ),
        Cmp::Shape => {
            assert_eq!(srv.first(), emb.first(), "kind mismatch for `{what}`");
            if srv.first() == Some(&b'*') {
                assert_eq!(
                    first_elem_count(srv),
                    first_elem_count(emb),
                    "array length mismatch for `{what}`"
                );
            }
        }
        Cmp::ScanShape => {
            assert_eq!(srv.first(), Some(&b'*'), "server SCAN reply not an array for `{what}`");
            assert_eq!(emb.first(), Some(&b'*'), "embedded SCAN reply not an array for `{what}`");
            assert_eq!(first_elem_count(srv), 2, "server SCAN envelope for `{what}`");
            assert_eq!(first_elem_count(emb), 2, "embedded SCAN envelope for `{what}`");
        }
        Cmp::IntSlack(slack) => {
            let parse = |r: &[u8]| -> i64 {
                assert_eq!(r.first(), Some(&b':'), "expected integer reply for `{what}`");
                std::str::from_utf8(&r[1..r.len() - 2]).expect("utf8").parse().expect("int")
            };
            let (a, b) = (parse(srv), parse(emb));
            assert!(
                (a - b).abs() <= *slack,
                "integer replies too far apart for `{what}`: server {a} vs embedded {b}"
            );
        }
    }
}

/// The deterministic sequence — applied IN ORDER to both engines, so
/// the keyspaces stay identical command by command.
// LOC-WAIVER: pure data table — one row per oracle command.
fn cases() -> Vec<(Vec<&'static [u8]>, Cmp)> {
    use Cmp::{Exact, IntSlack, ScanShape, Shape};
    fn c(argv: &[&'static [u8]], cmp: Cmp) -> (Vec<&'static [u8]>, Cmp) {
        (argv.to_vec(), cmp)
    }
    vec![
        // conn face
        c(&[b"PING"], Exact),
        c(&[b"PING", b"hello"], Exact),
        c(&[b"PING", b"a", b"b"], Exact),
        c(&[b"ECHO", b"hi"], Exact),
        c(&[b"ECHO"], Exact),
        c(&[b"PUBLISH", b"ch", b"payload"], Exact),
        c(&[b"PUBLISH", b"ch"], Exact),
        // strings
        c(&[b"SET", b"k", b"v1"], Exact),
        c(&[b"GET", b"k"], Exact),
        c(&[b"GET", b"missing"], Exact),
        c(&[b"GET"], Exact),
        c(&[b"SET", b"k"], Exact),
        c(&[b"APPEND", b"k", b"-tail"], Exact),
        c(&[b"STRLEN", b"k"], Exact),
        c(&[b"SETNX", b"k", b"other"], Exact),
        c(&[b"SETNX", b"k2", b"fresh"], Exact),
        c(&[b"GETSET", b"k2", b"swapped"], Exact),
        c(&[b"GETDEL", b"k2"], Exact),
        c(&[b"GETDEL", b"k2"], Exact),
        c(&[b"SET", b"n", b"10"], Exact),
        c(&[b"INCR", b"n"], Exact),
        c(&[b"INCRBY", b"n", b"5"], Exact),
        c(&[b"DECR", b"n"], Exact),
        c(&[b"DECRBY", b"n", b"2"], Exact),
        c(&[b"INCRBY", b"n", b"abc"], Exact),
        c(&[b"INCR", b"k"], Exact),
        c(&[b"INCRBYFLOAT", b"n", b"0.5"], Exact),
        c(&[b"MSET", b"m1", b"a", b"m2", b"b"], Exact),
        c(&[b"MSET", b"m1"], Exact),
        c(&[b"MGET", b"m1", b"m2", b"missing"], Exact),
        c(&[b"MGET"], Exact),
        c(&[b"SET", b"nxk", b"v", b"NX"], Exact),
        c(&[b"SET", b"nxk", b"v2", b"NX"], Exact),
        c(&[b"SET", b"nxk", b"v3", b"XX"], Exact),
        c(&[b"SET", b"nxk", b"v4", b"NX", b"XX"], Exact),
        c(&[b"SET", b"nxk", b"v5", b"BOGUS"], Exact),
        c(&[b"SET", b"exk", b"v", b"EX", b"0"], Exact),
        // keyspace basics
        c(&[b"TYPE", b"k"], Exact),
        c(&[b"TYPE", b"missing"], Exact),
        c(&[b"EXISTS", b"m1", b"m2", b"missing", b"m1"], Exact),
        c(&[b"DEL", b"m1"], Exact),
        c(&[b"UNLINK", b"m2", b"missing"], Exact),
        c(&[b"DEL"], Exact),
        c(&[b"TTL", b"k"], Exact),
        c(&[b"TTL", b"missing"], Exact),
        c(&[b"EXPIRE", b"k", b"100"], Exact),
        c(&[b"TTL", b"k"], IntSlack(1)),
        c(&[b"PTTL", b"k"], IntSlack(1500)),
        c(&[b"PERSIST", b"k"], Exact),
        c(&[b"PERSIST", b"k"], Exact),
        c(&[b"EXPIRE", b"missing", b"10"], Exact),
        c(&[b"EXPIRE", b"k", b"abc"], Exact),
        c(&[b"EXPIREAT", b"k", b"99999999999"], Exact),
        c(&[b"PEXPIREAT", b"k", b"99999999999999"], Exact),
        c(&[b"PEXPIRE", b"k", b"abc"], Exact),
        c(&[b"PERSIST", b"k"], Exact),
        // hashes
        c(&[b"HSET", b"h", b"f1", b"v1", b"f2", b"v2"], Exact),
        c(&[b"HSET", b"h", b"f1"], Exact),
        c(&[b"HGET", b"h", b"f1"], Exact),
        c(&[b"HGET", b"h", b"nope"], Exact),
        c(&[b"HEXISTS", b"h", b"f1"], Exact),
        c(&[b"HLEN", b"h"], Exact),
        c(&[b"HMGET", b"h", b"f1", b"nope"], Exact),
        c(&[b"HSETNX", b"h", b"f1", b"zz"], Exact),
        c(&[b"HSETNX", b"h", b"f3", b"v3"], Exact),
        c(&[b"HINCRBY", b"h", b"cnt", b"5"], Exact),
        c(&[b"HINCRBY", b"h", b"cnt", b"abc"], Exact),
        c(&[b"HDEL", b"h", b"f3", b"nope"], Exact),
        c(&[b"HDEL", b"h"], Exact),
        c(&[b"HSCAN", b"h", b"0"], Shape),
        c(&[b"HSCAN", b"h", b"abc"], Exact),
        // lists
        c(&[b"LPUSH", b"l", b"a", b"b", b"c"], Exact),
        c(&[b"LPUSH", b"l"], Exact),
        c(&[b"LLEN", b"l"], Exact),
        c(&[b"LRANGE", b"l", b"0", b"-1"], Exact),
        c(&[b"LRANGE", b"l", b"0", b"x"], Exact),
        c(&[b"LINDEX", b"l", b"1"], Exact),
        c(&[b"RPUSH", b"l", b"z"], Exact),
        c(&[b"LSET", b"l", b"0", b"top"], Exact),
        c(&[b"LSET", b"missing", b"0", b"v"], Exact),
        c(&[b"LREM", b"l", b"0", b"z"], Exact),
        c(&[b"LTRIM", b"l", b"0", b"1"], Exact),
        c(&[b"LPOP", b"l"], Exact),
        c(&[b"RPOP", b"l"], Exact),
        c(&[b"LPOP", b"l", b"2"], Exact),
        c(&[b"LPOP", b"missing"], Exact),
        c(&[b"LPOP", b"l", b"-1"], Exact),
        // sets
        c(&[b"SADD", b"s1", b"a", b"b", b"c"], Exact),
        c(&[b"SADD", b"s1"], Exact),
        c(&[b"SCARD", b"s1"], Exact),
        c(&[b"SISMEMBER", b"s1", b"a"], Exact),
        c(&[b"SISMEMBER", b"s1", b"x"], Exact),
        c(&[b"SREM", b"s1", b"c", b"x"], Exact),
        c(&[b"SMEMBERS", b"s1"], Shape),
        c(&[b"SADD", b"s2", b"b", b"d"], Exact),
        c(&[b"SINTER", b"s1", b"s2"], Shape),
        c(&[b"SUNION", b"s1", b"s2"], Shape),
        c(&[b"SDIFF", b"s1", b"s2"], Shape),
        c(&[b"SINTERSTORE", b"sd", b"s1", b"s2"], Exact),
        c(&[b"SUNIONSTORE", b"sd", b"s1", b"s2"], Exact),
        c(&[b"SDIFFSTORE", b"sd", b"s1", b"s2"], Exact),
        c(&[b"SPOP", b"missing"], Exact),
        c(&[b"SPOP", b"s2", b"1"], Shape),
        c(&[b"SRANDMEMBER", b"s1", b"2"], Shape),
        c(&[b"SPOP", b"s1", b"-1"], Exact),
        // zsets
        c(&[b"ZADD", b"z", b"1", b"a", b"2", b"b", b"1.5", b"c"], Exact),
        c(&[b"ZADD", b"z", b"1", b"a", b"2"], Exact),
        c(&[b"ZADD", b"z", b"nan", b"m"], Exact),
        c(&[b"ZADD", b"z", b"NX", b"XX", b"1", b"m"], Exact),
        c(&[b"ZADD", b"z", b"NX", b"9", b"a"], Exact),
        c(&[b"ZADD", b"z", b"GT", b"CH", b"99", b"b"], Exact),
        c(&[b"ZADD", b"z", b"INCR", b"1", b"b"], Exact),
        c(&[b"ZCARD", b"z"], Exact),
        c(&[b"ZSCORE", b"z", b"a"], Exact),
        c(&[b"ZSCORE", b"z", b"c"], Exact),
        c(&[b"ZSCORE", b"z", b"nope"], Exact),
        c(&[b"ZRANK", b"z", b"b"], Exact),
        c(&[b"ZRANK", b"z", b"nope"], Exact),
        c(&[b"ZRANGE", b"z", b"0", b"-1"], Exact),
        c(&[b"ZRANGE", b"z", b"0", b"-1", b"WITHSCORES"], Exact),
        c(&[b"ZRANGE", b"z", b"0", b"-1", b"BOGUS"], Exact),
        c(&[b"ZCOUNT", b"z", b"1", b"2"], Exact),
        c(&[b"ZCOUNT", b"z", b"(1", b"inf"], Exact),
        c(&[b"ZCOUNT", b"z", b"x", b"2"], Exact),
        c(&[b"ZINCRBY", b"z", b"0.5", b"a"], Exact),
        c(&[b"ZRANGEBYSCORE", b"z", b"1", b"2", b"WITHSCORES"], Exact),
        c(&[b"ZRANGEBYSCORE", b"z", b"(1", b"+inf"], Exact),
        c(&[b"ZRANGEBYSCORE", b"z", b"1", b"2", b"LIMIT", b"0", b"1"], Exact),
        c(&[b"ZREVRANGEBYSCORE", b"z", b"2", b"1"], Exact),
        c(&[b"ZPOPMIN", b"z"], Exact),
        c(&[b"ZPOPMIN", b"z", b"x"], Exact),
        c(&[b"ZPOPMIN.BELOW", b"z", b"100"], Exact),
        c(&[b"ZREMRANGEBYRANK", b"z", b"0", b"0"], Exact),
        c(&[b"ZREMRANGEBYSCORE", b"z", b"200", b"300"], Exact),
        c(&[b"ZADD", b"za", b"1", b"m1", b"2", b"m2"], Exact),
        c(&[b"ZADD", b"zb", b"10", b"m2", b"20", b"m3"], Exact),
        c(&[b"ZUNIONSTORE", b"zd", b"2", b"za", b"zb"], Exact),
        c(&[b"ZUNIONSTORE", b"zd", b"x", b"za"], Exact),
        c(&[b"ZUNIONSTORE", b"zd", b"2", b"za", b"zb", b"WEIGHTS", b"2", b"3"], Exact),
        c(&[b"ZUNIONSTORE", b"zd", b"2", b"za", b"zb", b"AGGREGATE", b"MAX"], Exact),
        c(&[b"ZINTERSTORE", b"zd", b"2", b"za", b"zb"], Exact),
        c(&[b"ZDIFFSTORE", b"zd", b"2", b"za", b"zb"], Exact),
        c(&[b"ZINTERCARD", b"2", b"za", b"zb"], Exact),
        c(&[b"ZINTERCARD", b"0", b"za"], Exact),
        c(&[b"ZRANGE", b"zd", b"0", b"-1", b"WITHSCORES"], Exact),
        c(&[b"ZSCAN", b"za", b"0"], Exact),
        c(&[b"ZSCAN", b"za", b"abc"], Exact),
        // keyspace sweeps
        c(&[b"DBSIZE"], Exact),
        c(&[b"KEYS", b"*"], Shape),
        c(&[b"KEYS"], Exact),
        c(&[b"SCAN", b"0"], ScanShape),
        c(&[b"SCAN", b"abc"], Exact),
        c(&[b"SCAN", b"0", b"COUNT", b"0"], Exact),
        c(&[b"RANDOMKEY"], Shape),
        c(&[b"RANDOMKEY", b"x"], Exact),
        c(&[b"SET", b"r1", b"v"], Exact),
        c(&[b"RENAME", b"r1", b"r2"], Exact),
        c(&[b"RENAME", b"missing", b"x"], Exact),
        c(&[b"RENAME"], Exact),
        c(&[b"SET", b"r3", b"v3"], Exact),
        c(&[b"RENAMENX", b"r3", b"r2"], Exact),
        c(&[b"RENAMENX", b"missing", b"x"], Exact),
        // digests (identical keyspace ⇒ identical checksum)
        c(&[b"PREFIX.DIGEST", b"z"], Exact),
        c(&[b"PREFIX.STATS", b"z"], Exact),
        // index/view error surface (embedded builds synchronously, so
        // only the pre-build paths compare byte-for-byte)
        c(&[b"IDX.QUERY", b"noidx", b"EQ", b"1"], Exact),
        c(&[b"IDX.COUNT", b"noidx", b"EQ", b"1"], Exact),
        c(&[b"IDX.DROP", b"noidx"], Exact),
        c(&[b"IDX.DROP"], Exact),
        c(&[b"IDX.CREATE", b"bad"], Exact),
        c(&[b"IDX.LIST"], Exact),
        c(&[b"VIEW.DROP", b"noview"], Exact),
        c(&[b"VIEW.QUERY", b"noview"], Exact),
        c(&[b"VIEW.LIST"], Exact),
        // unknown verb
        c(&[b"NoSuchVerbX"], Exact),
        c(&[b"NoSuchVerbX", b"arg"], Exact),
        // wipe
        c(&[b"FLUSHALL"], Exact),
        c(&[b"DBSIZE"], Exact),
    ]
}

#[test]
fn dispatch_matches_the_real_server() {
    let server_dir = kevy_tmpdir::TmpDir::new("dispatch-oracle-server");
    let _guard = spawn_server(server_dir.path());
    let mut sock = connect_with_retry();
    let mut buf = Vec::new();

    let store =
        Store::open(Config::default().with_ttl_reaper_manual()).expect("open embedded store");

    let mut checked = 0usize;
    for (argv, cmp) in cases() {
        let srv = server_reply(&mut sock, &mut buf, &argv);
        let emb = embedded_reply(&store, &argv);
        assert_match(&argv, &cmp, &srv, &emb);
        checked += 1;
    }
    assert!(checked >= 150, "oracle sequence unexpectedly short: {checked}");
}
