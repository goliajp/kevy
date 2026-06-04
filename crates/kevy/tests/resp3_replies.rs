//! RESP3 reply-shape migration (P3.1) — per-cmd dispatch_into_resp3
//! overrides. Conn negotiates `HELLO 3`, then a small set of commands
//! reply with the RESP3 shape (`%N` Map, `~N` Set, `,N` Double, `_`
//! Null). Every other cmd still emits RESP2 bytes — gradual migration
//! is spec-legal and is what each subsequent P3.x commit chips at.
//!
//! Each test pairs a V3 client + a V2 control to assert the V2 wire
//! is byte-for-byte unchanged after the migration (the "RESP2 client
//! pays nothing" guardrail).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn read_reply(s: &mut std::net::TcpStream, expected: &[u8]) {
    let mut buf = vec![0u8; expected.len()];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(
        &buf,
        expected,
        "expected {:?}, got {:?}",
        String::from_utf8_lossy(expected),
        String::from_utf8_lossy(&buf),
    );
}

/// Drain `n` bytes (loose). Used to skip an opaque ack.
fn skip_n(s: &mut std::net::TcpStream, n: usize) {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).unwrap();
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Server {
        let _gate = START_GATE.lock().unwrap_or_else(|e| e.into_inner());
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-resp3-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        let mut ready = false;
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready, "runtime did not come up");
        Server { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        s
    }

    /// Conn + HELLO 3 + drain ack. Returns a stream in V3 mode.
    fn v3_conn(&self) -> std::net::TcpStream {
        let mut c = self.connect();
        c.write_all(&req(&[b"HELLO", b"3"])).unwrap();
        // Drain the `%7\r\n…` HELLO 3 Map ack. ~150 B is plenty for kevy's body.
        let mut sink = vec![0u8; 256];
        let _ = c.read(&mut sink).unwrap();
        c
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn hgetall_returns_map_on_resp3() {
    let srv = Server::start(1);

    // V2 conn: HSET 2 fields, HGETALL replies as RESP2 array `*4`.
    let mut v2 = srv.connect();
    v2.write_all(&req(&[b"HSET", b"h", b"f1", b"v1", b"f2", b"v2"]))
        .unwrap();
    read_reply(&mut v2, b":2\r\n");
    v2.write_all(&req(&[b"HGETALL", b"h"])).unwrap();
    // RESP2: `*4\r\n$2\r\nf1\r\n$2\r\nv1\r\n$2\r\nf2\r\n$2\r\nv2\r\n`
    // (or f2/v2 first — HashMap iteration order. We compare bytes
    // permutation-tolerantly.)
    let mut v2_buf = vec![0u8; 36];
    v2.read_exact(&mut v2_buf).unwrap();
    assert!(v2_buf.starts_with(b"*4\r\n"), "V2 HGETALL must stay array-shaped");

    // V3 conn: HGETALL replies as RESP3 Map `%2`.
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"HGETALL", b"h"])).unwrap();
    let mut v3_head = [0u8; 4];
    v3.read_exact(&mut v3_head).unwrap();
    assert_eq!(&v3_head, b"%2\r\n", "V3 HGETALL must reply with Map header `%2`");
    // Drain the 2 pairs (4 bulk frames; each `$2\r\nXX\r\n` = 8 bytes).
    let mut v3_body = vec![0u8; 32];
    v3.read_exact(&mut v3_body).unwrap();
}

#[test]
fn zscore_returns_double_on_resp3() {
    let srv = Server::start(1);
    let mut v2 = srv.connect();
    v2.write_all(&req(&[b"ZADD", b"z", b"1.5", b"alice"])).unwrap();
    read_reply(&mut v2, b":1\r\n");

    // V2: ZSCORE returns a Bulk string ("$3\r\n1.5\r\n").
    v2.write_all(&req(&[b"ZSCORE", b"z", b"alice"])).unwrap();
    read_reply(&mut v2, b"$3\r\n1.5\r\n");
    // Missing member → nil bulk.
    v2.write_all(&req(&[b"ZSCORE", b"z", b"nobody"])).unwrap();
    read_reply(&mut v2, b"$-1\r\n");

    // V3: ZSCORE returns a Double (",1.5\r\n"); missing → Null ("_\r\n").
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"ZSCORE", b"z", b"alice"])).unwrap();
    read_reply(&mut v3, b",1.5\r\n");
    v3.write_all(&req(&[b"ZSCORE", b"z", b"nobody"])).unwrap();
    read_reply(&mut v3, b"_\r\n");
}

#[test]
fn zincrby_returns_double_on_resp3() {
    let srv = Server::start(1);
    let mut v2 = srv.connect();
    // V2 ZINCRBY: bulk string `"$1\r\n5\r\n"` (integer-valued).
    v2.write_all(&req(&[b"ZINCRBY", b"z", b"5", b"x"])).unwrap();
    read_reply(&mut v2, b"$1\r\n5\r\n");

    let mut v3 = srv.v3_conn();
    // V3 ZINCRBY: Double `,7\r\n` (integer-valued double, no decimal).
    v3.write_all(&req(&[b"ZINCRBY", b"z", b"2", b"x"])).unwrap();
    read_reply(&mut v3, b",7\r\n");
    // Fractional case: `,3.5\r\n`.
    v3.write_all(&req(&[b"ZINCRBY", b"z", b"-3.5", b"x"])).unwrap();
    read_reply(&mut v3, b",3.5\r\n");
}

#[test]
fn smembers_returns_set_on_resp3() {
    let srv = Server::start(1);
    let mut v2 = srv.connect();
    v2.write_all(&req(&[b"SADD", b"s", b"a", b"b", b"c"])).unwrap();
    read_reply(&mut v2, b":3\r\n");

    // V2: `*3\r\n$1\r\na\r\n…` (order non-deterministic).
    v2.write_all(&req(&[b"SMEMBERS", b"s"])).unwrap();
    let mut v2_head = [0u8; 4];
    v2.read_exact(&mut v2_head).unwrap();
    assert_eq!(&v2_head, b"*3\r\n");
    skip_n(&mut v2, 18); // 3 × `$1\r\nX\r\n` = 18 bytes.

    // V3: `~3\r\n…` (Set header).
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"SMEMBERS", b"s"])).unwrap();
    let mut v3_head = [0u8; 4];
    v3.read_exact(&mut v3_head).unwrap();
    assert_eq!(&v3_head, b"~3\r\n", "V3 SMEMBERS must reply with Set header `~3`");
    skip_n(&mut v3, 18);
}

#[test]
fn unmigrated_cmds_still_emit_resp2_on_v3_conn() {
    // P3 migrates one cmd shape per phase; cmds without an override
    // still go out as RESP2 bytes to V3 conns (gradual migration —
    // see the RESP3 spec section "Compatibility").
    let srv = Server::start(1);
    let mut v3 = srv.v3_conn();

    // GET stays bulk-string-shaped on V3.
    v3.write_all(&req(&[b"SET", b"k", b"value"])).unwrap();
    read_reply(&mut v3, b"+OK\r\n");
    v3.write_all(&req(&[b"GET", b"k"])).unwrap();
    read_reply(&mut v3, b"$5\r\nvalue\r\n");

    // INCR stays integer-shaped.
    v3.write_all(&req(&[b"INCR", b"counter"])).unwrap();
    read_reply(&mut v3, b":1\r\n");

    // HKEYS / HVALS still array-shaped (only HGETALL is migrated in P3.1).
    v3.write_all(&req(&[b"HSET", b"h", b"a", b"x"])).unwrap();
    read_reply(&mut v3, b":1\r\n");
    v3.write_all(&req(&[b"HKEYS", b"h"])).unwrap();
    read_reply(&mut v3, b"*1\r\n$1\r\na\r\n");
    v3.write_all(&req(&[b"HVALS", b"h"])).unwrap();
    read_reply(&mut v3, b"*1\r\n$1\r\nx\r\n");
}

#[test]
fn sinter_sunion_sdiff_return_set_on_resp3_cross_shard() {
    // Multi-key set algebra goes through the kevy-rt reduce layer
    // (finalize_gather), NOT the kevy dispatch_into chain. P3.2 plumbs
    // proto through PendingSlot → materialize → finalize_gather so the
    // SInter/SUnion/SDiff arm picks Set vs Array per the conn's proto
    // recorded at start_multi time.
    let srv = Server::start(4); // multi-shard exercises the cross-shard gather
    let mut v2 = srv.connect();
    v2.write_all(&req(&[b"SADD", b"a", b"x", b"y", b"z"])).unwrap();
    read_reply(&mut v2, b":3\r\n");
    v2.write_all(&req(&[b"SADD", b"b", b"y", b"z", b"w"])).unwrap();
    read_reply(&mut v2, b":3\r\n");

    // V3 conn: SINTER returns `~2` Set header (members y, z in any order).
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"SINTER", b"a", b"b"])).unwrap();
    let mut head = [0u8; 4];
    v3.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"~2\r\n", "V3 SINTER must use Set header");
    // Drain the 2 bulks (`$1\r\nX\r\n` = 7 bytes each).
    skip_n(&mut v3, 14);

    // SUNION: 4 distinct members → `~4`.
    v3.write_all(&req(&[b"SUNION", b"a", b"b"])).unwrap();
    let mut head = [0u8; 4];
    v3.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"~4\r\n");
    skip_n(&mut v3, 28);

    // SDIFF a \ b: just {x} → `~1`.
    v3.write_all(&req(&[b"SDIFF", b"a", b"b"])).unwrap();
    let mut head = [0u8; 4];
    v3.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"~1\r\n");
    skip_n(&mut v3, 7);

    // V2 control: SINTER stays as `*2` Array.
    v2.write_all(&req(&[b"SINTER", b"a", b"b"])).unwrap();
    let mut head = [0u8; 4];
    v2.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"*2\r\n");
    skip_n(&mut v2, 14);
}

#[test]
fn mget_stays_array_on_resp3() {
    // MGET is the OTHER multi-key gather but RESP3 keeps it array-shaped
    // (member order is significant per the MGET contract; Set is not
    // valid). Confirms finalize_gather's MGET arm doesn't get swept up
    // in the SInter/SUnion/SDiff Set-header switch.
    let srv = Server::start(4);
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"SET", b"a", b"1"])).unwrap();
    read_reply(&mut v3, b"+OK\r\n");
    v3.write_all(&req(&[b"SET", b"b", b"2"])).unwrap();
    read_reply(&mut v3, b"+OK\r\n");
    v3.write_all(&req(&[b"MGET", b"a", b"missing", b"b"])).unwrap();
    // Same array shape as V2: `*3\r\n$1\r\n1\r\n$-1\r\n$1\r\n2\r\n`.
    read_reply(&mut v3, b"*3\r\n$1\r\n1\r\n$-1\r\n$1\r\n2\r\n");
}

#[test]
fn config_get_returns_map_on_resp3() {
    // CONFIG GET returns a known-key/value pair table on both protos.
    // V2: `*2N\r\n[k, v, k, v, ...]` Array. V3: `%N\r\n[k, v, k, v, ...]` Map.
    let srv = Server::start(1);

    // V2 control: pattern `appendfsync` matches exactly 1 key.
    let mut v2 = srv.connect();
    v2.write_all(&req(&[b"CONFIG", b"GET", b"appendfsync"])).unwrap();
    let mut head = [0u8; 4];
    v2.read_exact(&mut head).unwrap();
    assert_eq!(&head[..3], b"*2\r", "V2 CONFIG GET must use Array header `*2N`");
    // Drain the 2 bulks.
    let mut sink = vec![0u8; 64];
    let _ = v2.read(&mut sink).unwrap();

    // V3: same pattern → Map of 1 pair → `%1\r\n`.
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"CONFIG", b"GET", b"appendfsync"])).unwrap();
    let mut head = [0u8; 4];
    v3.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"%1\r\n", "V3 CONFIG GET must use Map header `%1`");
    // Drain the (k, v) pair.
    let _ = v3.read(&mut sink).unwrap();
}

#[test]
fn zrange_withscores_returns_nested_array_on_resp3() {
    // V2 WITHSCORES: flat `*2N\r\n[m, s, m, s, ...]` interleaved bulks.
    // V3 WITHSCORES: `*N\r\n[*2 [m, score-double], ...]` nested arrays
    // (per RESP3 spec — each pair is a proper 2-tuple, score is a
    // typed Double).
    let srv = Server::start(1);
    let mut v2 = srv.connect();
    v2.write_all(&req(&[b"ZADD", b"z", b"1", b"a", b"2.5", b"b"]))
        .unwrap();
    read_reply(&mut v2, b":2\r\n");

    // V2 ZRANGE WITHSCORES — flat interleaved bulks.
    v2.write_all(&req(&[b"ZRANGE", b"z", b"0", b"-1", b"WITHSCORES"]))
        .unwrap();
    // *4 + bulk a + bulk 1 + bulk b + bulk 2.5
    read_reply(
        &mut v2,
        b"*4\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$3\r\n2.5\r\n",
    );

    // V3 ZRANGE WITHSCORES — nested [bulk, double] pairs.
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"ZRANGE", b"z", b"0", b"-1", b"WITHSCORES"]))
        .unwrap();
    // *2 + (*2 + bulk a + ,1) + (*2 + bulk b + ,2.5)
    read_reply(
        &mut v3,
        b"*2\r\n*2\r\n$1\r\na\r\n,1\r\n*2\r\n$1\r\nb\r\n,2.5\r\n",
    );

    // No-WITHSCORES form: same plain bulk array on both protos.
    v3.write_all(&req(&[b"ZRANGE", b"z", b"0", b"-1"])).unwrap();
    read_reply(&mut v3, b"*2\r\n$1\r\na\r\n$1\r\nb\r\n");
}

#[test]
fn zrangebyscore_withscores_returns_nested_array_on_resp3() {
    let srv = Server::start(1);
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"ZADD", b"zz", b"1", b"x", b"3", b"y"]))
        .unwrap();
    read_reply(&mut v3, b":2\r\n");

    // Range covers both members; WITHSCORES → nested pairs with Double.
    v3.write_all(&req(&[
        b"ZRANGEBYSCORE",
        b"zz",
        b"-inf",
        b"+inf",
        b"WITHSCORES",
    ]))
    .unwrap();
    read_reply(
        &mut v3,
        b"*2\r\n*2\r\n$1\r\nx\r\n,1\r\n*2\r\n$1\r\ny\r\n,3\r\n",
    );
}

#[test]
fn info_and_client_info_use_verbatim_on_resp3() {
    // V2: bulk string `$<len>\r\n<body>\r\n`.
    // V3: Verbatim string `=<len>\r\ntxt:<body>\r\n` so the client
    // knows it's plain text (`txt` 3-char fmt tag is RESP3 spec).
    let srv = Server::start(1);

    // V2 INFO: leading byte is `$` (bulk).
    let mut v2 = srv.connect();
    v2.write_all(&req(&[b"INFO", b"server"])).unwrap();
    let mut head = [0u8; 1];
    v2.read_exact(&mut head).unwrap();
    assert_eq!(head[0], b'$', "V2 INFO must stay bulk-string-shaped");
    // Drain the body line + payload.
    let mut sink = vec![0u8; 4096];
    let _ = v2.read(&mut sink).unwrap();

    // V3 INFO: leading byte is `=` (verbatim).
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"INFO", b"server"])).unwrap();
    v3.read_exact(&mut head).unwrap();
    assert_eq!(head[0], b'=', "V3 INFO must use Verbatim string");
    // Drain rest.
    let mut sink = vec![0u8; 4096];
    let n = v3.read(&mut sink).unwrap();
    // The body must start with `<len>\r\ntxt:` per RESP3 verbatim wire.
    let s = &sink[..n];
    let crlf = s.iter().position(|&b| b == b'\n').unwrap();
    assert!(
        s[crlf + 1..crlf + 5] == *b"txt:",
        "V3 INFO body must start with `txt:` fmt tag, got {:?}",
        String::from_utf8_lossy(&s[crlf + 1..crlf + 16])
    );

    // CLIENT INFO same shape change.
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"CLIENT", b"INFO"])).unwrap();
    let mut head = [0u8; 1];
    v3.read_exact(&mut head).unwrap();
    assert_eq!(head[0], b'=', "V3 CLIENT INFO must use Verbatim string");

    // CLIENT LIST same shape change.
    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"CLIENT", b"LIST"])).unwrap();
    let mut head = [0u8; 1];
    v3.read_exact(&mut head).unwrap();
    assert_eq!(head[0], b'=', "V3 CLIENT LIST must use Verbatim string");
}

#[test]
fn pubsub_message_uses_push_frame_on_resp3() {
    // V2 subscriber: `*3\r\nmessage\r\n<chan>\r\n<payload>\r\n` array.
    // V3 subscriber: `>3\r\nmessage\r\n<chan>\r\n<payload>\r\n` push.
    // Mixed V2 + V3 subscribers on the same channel each get the right
    // shape (per-conn proto applied at deliver_publish).
    let srv = Server::start(1);

    let mut v2 = srv.connect();
    v2.write_all(&req(&[b"SUBSCRIBE", b"news"])).unwrap();
    read_reply(
        &mut v2,
        b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n",
    );

    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"SUBSCRIBE", b"news"])).unwrap();
    // V3 subscribe ack itself is push-framed: `>3\r\n…`.
    read_reply(
        &mut v3,
        b">3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n",
    );

    let mut pub_ = srv.connect();
    pub_.write_all(&req(&[b"PUBLISH", b"news", b"hello"])).unwrap();
    read_reply(&mut pub_, b":2\r\n");

    // V2 sub: message arrives as `*3` array.
    read_reply(
        &mut v2,
        b"*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n",
    );
    // V3 sub: same body, `>3` push prefix.
    read_reply(
        &mut v3,
        b">3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n",
    );
}

#[test]
fn pmessage_uses_push_frame_on_resp3() {
    // PSUBSCRIBE ack + pmessage delivery, both push-framed on V3.
    let srv = Server::start(1);

    let mut v3 = srv.v3_conn();
    v3.write_all(&req(&[b"PSUBSCRIBE", b"news.*"])).unwrap();
    read_reply(
        &mut v3,
        b">3\r\n$10\r\npsubscribe\r\n$6\r\nnews.*\r\n:1\r\n",
    );

    let mut pub_ = srv.connect();
    pub_.write_all(&req(&[b"PUBLISH", b"news.tech", b"hi"]))
        .unwrap();
    read_reply(&mut pub_, b":1\r\n");
    read_reply(
        &mut v3,
        b">4\r\n$8\r\npmessage\r\n$6\r\nnews.*\r\n$9\r\nnews.tech\r\n$2\r\nhi\r\n",
    );
}

#[test]
fn v2_wire_byte_for_byte_unchanged_after_resp3_migration() {
    // Critical guardrail: every V2 cmd test in the existing suite
    // (sharded.rs, cmd_matrix.rs, commands.rs) already asserts the
    // RESP2 bytes. This test focuses on the 4 cmds migrated in P3.1
    // and confirms the V2 conn still gets identical bytes vs. their
    // pre-P3 form.
    let srv = Server::start(1);
    let mut c = srv.connect();

    c.write_all(&req(&[b"HSET", b"hh", b"x", b"y"])).unwrap();
    read_reply(&mut c, b":1\r\n");
    c.write_all(&req(&[b"HGETALL", b"hh"])).unwrap();
    read_reply(&mut c, b"*2\r\n$1\r\nx\r\n$1\r\ny\r\n");

    c.write_all(&req(&[b"ZADD", b"zz", b"2.5", b"m"])).unwrap();
    read_reply(&mut c, b":1\r\n");
    c.write_all(&req(&[b"ZSCORE", b"zz", b"m"])).unwrap();
    read_reply(&mut c, b"$3\r\n2.5\r\n");
    c.write_all(&req(&[b"ZSCORE", b"zz", b"nope"])).unwrap();
    read_reply(&mut c, b"$-1\r\n");
    c.write_all(&req(&[b"ZINCRBY", b"zz", b"1.5", b"m"])).unwrap();
    read_reply(&mut c, b"$1\r\n4\r\n");

    c.write_all(&req(&[b"SADD", b"ss", b"only"])).unwrap();
    read_reply(&mut c, b":1\r\n");
    c.write_all(&req(&[b"SMEMBERS", b"ss"])).unwrap();
    read_reply(&mut c, b"*1\r\n$4\r\nonly\r\n");
}
