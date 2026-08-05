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
    // ALWAYS run the build — cargo is incremental, so a fresh binary
    // costs nothing and a stale one costs an afternoon: build-if-absent
    // served a cached pre-4.1 binary on CI (restored target/ cache) and
    // twice on a dev box, each time as a baffling oracle mismatch
    // against a server that "couldn't" be emitting that reply.
    let status = Command::new("cargo")
        .args(["build", "-p", "kevy"])
        .current_dir(&root)
        .status()
        .expect("spawn cargo build -p kevy");
    assert!(status.success(), "cargo build -p kevy failed");
    root.join("target/debug/kevy")
}

fn spawn_server(dir: &std::path::Path) -> ServerGuard {
    let child = Command::new(server_binary())
        // One shard: this oracle compares DISPATCH, and the store it
        // compares against is single-shard, so a multi-shard server
        // adds a variable and N times the startup cost — three of
        // those spawning at once under the workspace suite is what
        // starved the accept loop.
        .args(["--threads", "1", "--port", &PORT.to_string(), "--dir"])
        .arg(dir)
        .current_dir(dir) // never run a server from the repo root
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kevy server");
    ServerGuard(child)
}

/// How long to wait for a spawned server to accept — 10 s, scaled by
/// `KEVY_TEST_PATIENCE`, the same knob the replication suite uses.
///
/// A fixed 10 s is enough on an idle machine and not enough under
/// `cargo test --workspace`, where this oracle spawns a real server
/// while the rest of the suite has the cores. That lost three runs in
/// one day, each time passing on its own immediately afterwards. The
/// budget now tracks machine speed instead of being raised blind: a
/// server that is genuinely wedged still fails at 10 s in a normal
/// build.
fn accept_budget() -> Duration {
    let mult: f64 = std::env::var("KEVY_TEST_PATIENCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    Duration::from_secs_f64(10.0 * mult)
}

fn connect_with_retry_on(port: u16) -> TcpStream {
    let deadline = std::time::Instant::now() + accept_budget();
    loop {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
            return s;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "kevy server did not accept on port {port} within {:?} — \
             a loaded runner widens this with KEVY_TEST_PATIENCE",
            accept_budget()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn connect_with_retry() -> TcpStream {
    connect_with_retry_on(PORT)
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
        // scalar VALUES surface (capacity arc G1) — the create-time and
        // grammar-time verdicts, deterministic on both engines.
        c(&[b"IDX.CREATE", b"vagg", b"ON", b"PREFIX", b"g:", b"FIELD", b"n", b"TYPE", b"i64",
            b"KIND", b"agg", b"GROUPBY", b"c", b"VALUES", b"c"], Exact),
        c(&[b"IDX.CREATE", b"vbad", b"ON", b"PREFIX", b"g:", b"FIELD", b"n", b"TYPE", b"i64",
            b"KIND", b"range", b"VALUES", b"a", b"b", b"TYPES", b"i64"], Exact),
        c(&[b"IDX.CREATE", b"vbad", b"ON", b"PREFIX", b"g:", b"FIELD", b"n", b"TYPE", b"i64",
            b"KIND", b"range", b"VALUES", b"a", b"TYPES", b"bogus"], Exact),
        c(&[b"IDX.QUERY", b"noidx", b"RANGE", b"0", b"9", b"FILTER", b"f", b"EQ", b"1"], Exact),
        c(&[b"IDX.QUERY", b"noidx", b"RANGE", b"0", b"9", b"SORT", b"f", b"ASC"], Exact),
        c(&[b"IDX.QUERY", b"noidx", b"RANGE", b"0", b"9", b"SORT", b"f"], Exact),
        c(&[b"IDX.QUERY", b"noidx", b"RANGE", b"0", b"9", b"SORT", b"f", b"SIDEWAYS"], Exact),
        c(&[b"IDX.QUERY", b"noidx", b"RANGE", b"0", b"9", b"CURSOR", b"0", b"SORT", b"f", b"ASC"], Exact),
        c(&[b"IDX.QUERY", b"noidx", b"RANGE", b"0", b"9", b"CURSOR", b"0", b"OFFSET", b"5"], Exact),
        c(&[b"IDX.QUERY", b"noidx", b"RANGE", b"0", b"9", b"FACET"], Exact),
        c(&[b"IDX.COUNT", b"noidx", b"RANGE", b"0", b"9", b"FILTER", b"f", b"EQ", b"1"], Exact),
        c(&[b"VIEW.DROP", b"noview"], Exact),
        c(&[b"VIEW.QUERY", b"noview"], Exact),
        c(&[b"VIEW.LIST"], Exact),
        // TABLE.* declaration surface (capacity arc T7) — grammar and
        // refusal parity; the success/query surface (which needs the
        // server's tick-driven backfill) lives in its own polled test.
        c(&[b"TABLE.DECLARE", b"bad"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"id", b"COLUMN", b"id", b"uuid"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"nope", b"COLUMN", b"id", b"str"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"COLUMN", b"id", b"i64"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"INDEX", b"ghost", b"RANGE"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"INDEX", b"id", b"agg"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"INDEX", b"id", b"RANGE", b"VALUES", b"ghost"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"ORDERPATH", b"op", b"ON", b"ghost"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"ORDERPATH", b"op", b"BY", b"id"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"COLUMN", b"n", b"i64",
            b"INDEX", b"n", b"RANGE",
            b"ORDERPATH", b"byn", b"ON", b"id", b"THEN", b"n", b"DESC"], Exact),
        // WINDOW clause — refusal and success parity, and the LIST
        // row's window cell on both faces.
        c(&[b"TABLE.DECLARE", b"tw", b"PREFIX", b"tw:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"COLUMN", b"at", b"i64",
            b"WINDOW", b"at", b"SPAN", b"90", b"BUCKET", b"1"], Exact),
        c(&[b"TABLE.DECLARE", b"tw", b"PREFIX", b"tw:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"WINDOW", b"id", b"SPAN", b"90", b"BUCKET", b"1"], Exact),
        c(&[b"TABLE.DECLARE", b"tw", b"PREFIX", b"tw:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"COLUMN", b"at", b"i64", b"INDEX", b"at", b"RANGE",
            b"WINDOW", b"at", b"SPAN", b"0", b"BUCKET", b"1"], Exact),
        c(&[b"TABLE.DECLARE", b"tw", b"PREFIX", b"tw:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"COLUMN", b"at", b"i64", b"INDEX", b"at", b"RANGE",
            b"WINDOW", b"at", b"SPAN", b"90"], Exact),
        c(&[b"TABLE.DECLARE", b"tw", b"PREFIX", b"tw:", b"PK", b"id", b"COLUMN", b"id", b"str",
            b"COLUMN", b"at", b"i64", b"INDEX", b"at", b"RANGE",
            b"WINDOW", b"at", b"SPAN", b"90", b"BUCKET", b"7"], Exact),
        c(&[b"TABLE.DECLARE", b"t1", b"PREFIX", b"tt:", b"PK", b"id", b"COLUMN", b"id", b"str"], Exact),
        c(&[b"TABLE.LIST"], Exact),
        c(&[b"TABLE.LIST", b"extra"], Exact),
        c(&[b"TABLE.VERIFY"], Exact),
        c(&[b"TABLE.VERIFY", b"nope"], Exact),
        c(&[b"TABLE.DROP"], Exact),
        c(&[b"TABLE.DROP", b"missing"], Exact),
        c(&[b"TABLE.DROP", b"t1"], Exact),
        c(&[b"TABLE.LIST"], Exact),
        // WHERE grammar refusals (index-independent shapes).
        c(&[b"IDX.QUERY", b"noidx", b"WHERE", b"a", b"EQ", b"1"], Exact),
        c(&[b"IDX.QUERY", b"noidx", b"WHERE", b"LIMIT", b"5"], Exact),
        c(&[b"IDX.QUERY", b"noidx", b"WHERE", b"a", b"NEQ", b"1"], Exact),
        c(&[b"IDX.COUNT", b"noidx", b"WHERE", b"a", b"EQ", b"1"], Exact),
        // unknown verb
        c(&[b"NoSuchVerbX"], Exact),
        c(&[b"NoSuchVerbX", b"arg"], Exact),
        // wipe
        c(&[b"FLUSHALL"], Exact),
        c(&[b"DBSIZE"], Exact),
    ]
}

/// The scalar VALUES / FILTER / SORT / DISTINCT / FACET / OFFSET
/// success surface, byte-compared on a READY index. Runs on its own
/// server (the index is created over an EMPTY domain so the server's
/// tick-driven backfill finishes immediately; the poll below waits for
/// it, then every row arrives through the write hook on both engines
/// and the claused replies must match byte-for-byte).
#[test]
fn scalar_values_clauses_match_the_real_server() {
    const PORT2: u16 = 6098;
    let server_dir = kevy_tmpdir::TmpDir::new("dispatch-oracle-values");
    let child = Command::new(server_binary())
        // One shard: this oracle compares DISPATCH, and the store it
        // compares against is single-shard, so a multi-shard server
        // adds a variable and N times the startup cost — three of
        // those spawning at once under the workspace suite is what
        // starved the accept loop.
        .args(["--threads", "1", "--port", &PORT2.to_string(), "--dir"])
        .arg(server_dir.path())
        .current_dir(server_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kevy server");
    let _guard = ServerGuard(child);
    let mut sock = connect_with_retry_on(PORT2);
    let mut buf = Vec::new();
    let store =
        Store::open(Config::default().with_ttl_reaper_manual()).expect("open embedded store");

    let create: Vec<&[u8]> = vec![
        b"IDX.CREATE", b"vals", b"ON", b"PREFIX", b"v:", b"FIELD", b"age", b"TYPE", b"i64",
        b"KIND", b"range", b"VALUES", b"city", b"price", b"TYPES", b"str", b"i64",
    ];
    let srv = server_reply(&mut sock, &mut buf, &create);
    let emb = embedded_reply(&store, &create);
    assert_eq!(srv, b"+OK\r\n");
    assert_eq!(emb, b"+OK\r\n");
    // Wait out the server's tick-driven backfill (empty domain — one
    // tick per shard); the embedded build is synchronous.
    for _ in 0..200 {
        let r = server_reply(&mut sock, &mut buf, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"0"]);
        if !r.starts_with(b"-INDEXBUILDING") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // LOC-WAIVER: pure data table — one row per oracle command.
    let cases: Vec<Vec<&'static [u8]>> = vec![
        vec![b"HSET", b"v:1", b"age", b"10", b"city", b"tokyo", b"price", b"5"],
        vec![b"HSET", b"v:2", b"age", b"20", b"city", b"osaka", b"price", b"3"],
        vec![b"HSET", b"v:3", b"age", b"30", b"price", b"8"],
        vec![b"HSET", b"v:4", b"age", b"40", b"city", b"tokyo"],
        vec![b"HSET", b"v:5", b"age", b"50", b"city", b"kyoto", b"price", b"3"],
        vec![b"HSET", b"v:6", b"age", b"60", b"city", b"osaka", b"price", b"x"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"price", b"RANGE", b"0", b"6"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"SORT", b"city", b"ASC"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"SORT", b"city", b"DESC"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"SORT", b"price", b"ASC"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"DISTINCT", b"city"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FACET", b"city", b"LIMIT", b"2"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"price", b"RANGE", b"0", b"6", b"FACET", b"city", b"price"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"DISTINCT", b"city", b"FACET", b"city"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"OFFSET", b"2", b"LIMIT", b"2"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"OFFSET", b"100"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"SORT", b"price", b"DESC", b"DISTINCT", b"city", b"FILTER", b"city", b"RANGE", b"a", b"z"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FIELDS", b"city", b"price"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"SORT", b"city", b"ASC", b"FIELDS", b"price"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"CURSOR", b"0", b"SORT", b"city", b"ASC"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"nope", b"EQ", b"1"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"price", b"EQ", b"abc"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"SORT", b"nope", b"ASC"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"DISTINCT", b"nope"],
        vec![b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FACET", b"nope"],
        vec![b"IDX.COUNT", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo"],
        vec![b"IDX.COUNT", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"price", b"RANGE", b"0", b"6"],
        vec![b"IDX.COUNT", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo", b"FILTER", b"price", b"RANGE", b"0", b"6"],
        vec![b"IDX.COUNT", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"nope", b"EQ", b"1"],
        vec![b"IDX.COUNT", b"vals", b"RANGE", b"0", b"100", b"SORT", b"city", b"ASC"],
        vec![b"IDX.COUNT", b"vals", b"RANGE", b"0", b"100", b"OFFSET", b"2"],
        vec![b"IDX.COUNT", b"vals", b"EQ", b"10", b"FILTER", b"city", b"EQ", b"tokyo"],
    ];
    for argv in &cases {
        let srv = server_reply(&mut sock, &mut buf, argv);
        let emb = embedded_reply(&store, argv);
        assert_match(argv, &Cmp::Exact, &srv, &emb);
    }
}

/// The TABLE.* success surface + composite WHERE queries, byte-compared
/// on a READY table (own server; the poll waits out the tick-driven
/// backfill of the compiled indexes; ≤ 40 rows so the bounded VERIFY
/// spot check samples every row on both engines and the counters agree
/// exactly).
#[test]
fn table_surface_matches_the_real_server() {
    const PORT3: u16 = 6099;
    let server_dir = kevy_tmpdir::TmpDir::new("dispatch-oracle-table");
    let child = Command::new(server_binary())
        // One shard: this oracle compares DISPATCH, and the store it
        // compares against is single-shard, so a multi-shard server
        // adds a variable and N times the startup cost — three of
        // those spawning at once under the workspace suite is what
        // starved the accept loop.
        .args(["--threads", "1", "--port", &PORT3.to_string(), "--dir"])
        .arg(server_dir.path())
        .current_dir(server_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kevy server");
    let _guard = ServerGuard(child);
    let mut sock = connect_with_retry_on(PORT3);
    let mut buf = Vec::new();
    let store =
        Store::open(Config::default().with_ttl_reaper_manual()).expect("open embedded store");

    let declare: Vec<&[u8]> = vec![
        b"TABLE.DECLARE", b"user", b"PREFIX", b"u:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"dept", b"str", b"COLUMN", b"age", b"i64",
        b"INDEX", b"age", b"RANGE", b"VALUES", b"dept",
        b"ORDERPATH", b"by_dept_age", b"ON", b"dept", b"THEN", b"age", b"DESC",
    ];
    assert_eq!(server_reply(&mut sock, &mut buf, &declare), b"+OK\r\n");
    assert_eq!(embedded_reply(&store, &declare), b"+OK\r\n");
    // Wait out the server's tick-driven backfill (empty domain).
    for _ in 0..200 {
        let r = server_reply(&mut sock, &mut buf, &[b"IDX.QUERY", b"user.age", b"RANGE", b"0", b"0"]);
        let r2 = server_reply(
            &mut sock, &mut buf,
            &[b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"x"],
        );
        if !r.starts_with(b"-INDEXBUILDING") && !r2.starts_with(b"-INDEXBUILDING") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // LOC-WAIVER: pure data table — one row per oracle command.
    let cases: Vec<Vec<&'static [u8]>> = vec![
        vec![b"HSET", b"u:1", b"id", b"1", b"dept", b"eng", b"age", b"30"],
        vec![b"HSET", b"u:2", b"id", b"2", b"dept", b"eng", b"age", b"45"],
        vec![b"HSET", b"u:3", b"id", b"3", b"dept", b"ops", b"age", b"25"],
        vec![b"HSET", b"u:4", b"id", b"4", b"dept", b"eng", b"age", b"38"],
        vec![b"HSET", b"u:5", b"id", b"5", b"dept", b"ops", b"age", b"52"],
        vec![b"HSET", b"u:6", b"id", b"6", b"age", b"99"], // dept missing → excluded from the composite
        vec![b"TABLE.LIST"],
        vec![b"TABLE.VERIFY", b"user"],
        // compiled single-column paths
        vec![b"IDX.QUERY", b"user.age", b"RANGE", b"30", b"50"],
        vec![b"IDX.QUERY", b"user.age", b"RANGE", b"0", b"100", b"FILTER", b"dept", b"EQ", b"eng"],
        // composite WHERE: equality, equality+range, DESC ordering
        vec![b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"eng"],
        vec![b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"eng", b"RANGE", b"age", b"31", b"46"],
        vec![b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"ops", b"LIMIT", b"1"],
        vec![b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"eng", b"FIELDS", b"age", b"dept"],
        vec![b"IDX.COUNT", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"eng"],
        // WHERE refusal surface on a live catalog
        vec![b"IDX.QUERY", b"user.age", b"WHERE", b"dept", b"EQ", b"eng"],
        vec![b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"ghost", b"EQ", b"1"],
        vec![b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"age", b"EQ", b"30"],
        vec![b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"eng", b"RANGE", b"age", b"x", b"46"],
        // drop cascades to the compiled indexes
        vec![b"TABLE.DROP", b"user"],
        vec![b"TABLE.LIST"],
        vec![b"IDX.QUERY", b"user.age", b"EQ", b"30"],
        vec![b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"eng"],
    ];
    for argv in &cases {
        let srv = server_reply(&mut sock, &mut buf, argv);
        let emb = embedded_reply(&store, argv);
        assert_match(argv, &Cmp::Exact, &srv, &emb);
    }
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
