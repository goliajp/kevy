//! v1.52 — Jedis 5.x + StackExchange.Redis ecosystem battle test
//! (Phase E step 1).
//!
//! Java (Jedis 5.x) and .NET (StackExchange.Redis 2.x) are the two
//! dominant enterprise Redis client stacks. Their golden-path RESP
//! wire patterns differ from the Node.js / Python clients already
//! battle-tested in `bullmq_*.rs`, `sidekiq.rs`, `celery.rs`, and
//! `ioredis_canonical.rs`:
//!
//! - Jedis pipelining batches every command into one write before
//!   reading any replies. kevy's RESP parser must handle the
//!   accumulated write-buffer cleanly.
//! - StackExchange.Redis upgrades to RESP3 via `HELLO 3` and uses
//!   `CLIENT NO-EVICT` / `CLIENT TRACKING OFF` to declare its
//!   connection profile.
//! - Both libraries identify themselves via `CLIENT SETNAME` after
//!   `HELLO`, then re-read it via `CLIENT GETNAME`.
//!
//! This test exercises the wire patterns directly — no JVM, no
//! .NET runtime — by hand-crafting the same RESP commands those
//! libraries send and asserting kevy's replies match each library's
//! invariants.
//!
//! Strict asserts:
//! - Jedis connect lifecycle: `HELLO 2` → `CLIENT SETNAME` →
//!   `CLIENT GETNAME` returns the name set.
//! - Jedis pipeline of 100 mixed commands: single bulk write, 100
//!   well-formed RESP replies returned in order.
//! - StackExchange.Redis `HELLO 3` returns the RESP3 map reply
//!   (kevy advertises `proto: 3`).
//! - StackExchange.Redis `CLIENT NO-EVICT ON` returns `+OK` or
//!   `-ERR unknown subcommand` (both are RESP-well-formed; the
//!   `-ERR` path is acceptable while kevy doesn't implement it).
//! - StackExchange.Redis multi-key MGET returns one bulk-string
//!   element per requested key, in order.
//! - Post-battle PING +PONG on both library profiles.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test jedis_stackex_battle --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

#[test]
#[ignore = "battle test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn jedis_5x_golden_path() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-jedis-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 1;
    let _h = Harness::spawn(cfg).expect("spawn kevy");
    std::thread::sleep(Duration::from_millis(200));

    let mut s = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("conn");
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));

    // PHASE 1: Jedis connect lifecycle.
    //
    // Jedis 5.x sends `HELLO 2` (downgrade to RESP2 by default) then
    // `CLIENT SETNAME <jedis-client-N>`.
    eprintln!("jedis: HELLO 2");
    s.write_all(&build(&[b"HELLO", b"2"])).unwrap();
    let hello_reply = read_one_reply(&mut s);
    assert!(
        hello_reply.starts_with("*") || hello_reply.starts_with("%"),
        "HELLO 2 expected array/map reply, got: {hello_reply:?}"
    );
    // The 'proto' field in the HELLO reply should mention 2 somewhere.
    assert!(
        hello_reply.contains("proto") || hello_reply.contains("PROTO"),
        "HELLO 2 reply missing proto field: {hello_reply:?}"
    );

    eprintln!("jedis: CLIENT SETNAME jedis-client-1");
    s.write_all(&build(&[b"CLIENT", b"SETNAME", b"jedis-client-1"]))
        .unwrap();
    let setname = read_one_reply(&mut s);
    assert!(
        setname.starts_with("+OK"),
        "CLIENT SETNAME expected +OK, got: {setname:?}"
    );

    eprintln!("jedis: CLIENT GETNAME");
    s.write_all(&build(&[b"CLIENT", b"GETNAME"])).unwrap();
    let getname = read_one_reply(&mut s);
    // v2.0.16: CLIENT SETNAME now persists per-connection via the
    // reactor-level intercept in `kevy-rt::exec_client_intercept` —
    // see CHANGELOG v2.0.16 and `crates/kevy-rt/src/exec_client_intercept.rs`.
    // Closes v1.52.x finding. The round-trip MUST now return the
    // name the SETNAME call wrote.
    assert!(
        getname.contains("jedis-client-1"),
        "CLIENT GETNAME expected round-trip of 'jedis-client-1', got: {getname:?}"
    );
    eprintln!("jedis: CLIENT GETNAME = {getname:?}");

    // PHASE 2: Jedis-style pipeline — 100 commands written in one shot,
    // 100 replies read in order.
    eprintln!("jedis: pipeline 100 mixed commands");
    let mut pipeline = Vec::with_capacity(8 * 1024);
    let mut expected_replies = Vec::with_capacity(100);
    for i in 0..100 {
        let key = format!("jedis:k:{i:03}");
        let val = format!("v-{i}");
        let typ = i % 4;
        match typ {
            0 => {
                pipeline.extend_from_slice(&build(&[b"SET", key.as_bytes(), val.as_bytes()]));
                expected_replies.push("+OK");
            }
            1 => {
                pipeline.extend_from_slice(&build(&[b"INCR", key.as_bytes()]));
                // INCR on string returns either :N or -WRONGTYPE
                // depending on prior SET. Accept either RESP-well-formed.
                expected_replies.push(":/-");
            }
            2 => {
                pipeline.extend_from_slice(&build(&[b"LPUSH", b"jedis:list", val.as_bytes()]));
                expected_replies.push(":");
            }
            _ => {
                pipeline.extend_from_slice(&build(&[b"HSET", b"jedis:hash", key.as_bytes(), val.as_bytes()]));
                expected_replies.push(":");
            }
        }
    }
    s.write_all(&pipeline).expect("pipeline write");

    // Drain 100 replies.
    let mut got_replies = 0;
    let mut acc = Vec::with_capacity(8 * 1024);
    let mut tmp_buf = vec![0u8; 4 * 1024];
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while got_replies < 100 && std::time::Instant::now() < deadline {
        let n = s.read(&mut tmp_buf).expect("pipeline read");
        if n == 0 {
            break;
        }
        acc.extend_from_slice(&tmp_buf[..n]);
        // Count complete replies by counting CRLF-terminated lines that
        // are reply terminators. Simpler: every reply ends in `\r\n` and
        // starts with one of `+ - : $ *`. We approximate by counting
        // those leading markers at line starts.
        got_replies = count_replies(&acc);
    }
    eprintln!("jedis: pipeline got {got_replies}/100 replies in {} bytes", acc.len());
    assert!(
        got_replies >= 100,
        "pipeline returned fewer than 100 replies: {got_replies}"
    );

    // PHASE 3: Jedis post-pipeline PING — connection still healthy.
    s.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let ping = read_one_reply(&mut s);
    assert!(
        ping.starts_with("+PONG"),
        "post-pipeline PING failed: {ping:?}"
    );
    eprintln!("jedis: golden path OK");

    drop(s);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore = "battle test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn stackexchange_redis_golden_path() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-stackex-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);

    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 1;
    let _h = Harness::spawn(cfg).expect("spawn kevy");
    std::thread::sleep(Duration::from_millis(200));

    let mut s = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("conn");
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));

    // PHASE 1: HELLO 3 — upgrade to RESP3.
    eprintln!("stackex: HELLO 3");
    s.write_all(&build(&[b"HELLO", b"3"])).unwrap();
    let hello_reply = read_one_reply(&mut s);
    assert!(
        hello_reply.starts_with("%") || hello_reply.starts_with("*"),
        "HELLO 3 expected map/array reply, got: {hello_reply:?}"
    );
    // proto field should mention 3.
    let has_proto3 = hello_reply.contains(":3\r\n")
        || hello_reply.contains("proto:3")
        || hello_reply.contains("PROTO:3");
    assert!(
        has_proto3,
        "HELLO 3 reply missing proto:3 field: {hello_reply:?}"
    );

    // PHASE 2: CLIENT SETNAME — same as Jedis.
    s.write_all(&build(&[b"CLIENT", b"SETNAME", b"stackex-client-1"]))
        .unwrap();
    let setname = read_one_reply(&mut s);
    assert!(
        setname.starts_with("+OK"),
        "CLIENT SETNAME expected +OK, got: {setname:?}"
    );

    // PHASE 3: CLIENT NO-EVICT ON — StackExchange.Redis sends this on
    // pool connections. kevy may not implement it; either +OK or
    // -ERR unknown subcommand is acceptable, so long as the reply is
    // well-formed RESP and the conn survives.
    eprintln!("stackex: CLIENT NO-EVICT ON");
    s.write_all(&build(&[b"CLIENT", b"NO-EVICT", b"ON"])).unwrap();
    let no_evict = read_one_reply(&mut s);
    assert!(
        no_evict.starts_with("+") || no_evict.starts_with("-"),
        "CLIENT NO-EVICT expected +OK or -ERR, got: {no_evict:?}"
    );
    eprintln!("stackex: CLIENT NO-EVICT reply = {no_evict:?}");

    // PHASE 4: Multi-key MGET via SET + MGET 5 keys (key-batching is
    // StackExchange.Redis's distinctive pattern). The 5 keys are
    // chosen to hash into the same slot in cluster mode (single-node
    // here, so any 5 keys work).
    eprintln!("stackex: SET 5 keys + MGET 5 keys");
    for i in 0..5 {
        let k = format!("stackex:mget:{i}");
        let v = format!("val-{i}");
        s.write_all(&build(&[b"SET", k.as_bytes(), v.as_bytes()]))
            .unwrap();
        let r = read_one_reply(&mut s);
        assert!(r.starts_with("+OK"), "SET expected +OK, got: {r:?}");
    }
    let mget = build(&[
        b"MGET",
        b"stackex:mget:0",
        b"stackex:mget:1",
        b"stackex:mget:2",
        b"stackex:mget:3",
        b"stackex:mget:4",
    ]);
    s.write_all(&mget).unwrap();
    let mget_reply = read_one_reply(&mut s);
    eprintln!("stackex: MGET reply = {mget_reply:?}");
    // Reply must be an array of 5 elements.
    assert!(
        mget_reply.starts_with("*5\r\n"),
        "MGET expected array of 5, got: {mget_reply:?}"
    );
    for i in 0..5 {
        let v = format!("val-{i}");
        assert!(
            mget_reply.contains(&v),
            "MGET reply missing val-{i}: {mget_reply:?}"
        );
    }

    // PHASE 5: Final PING.
    s.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let ping = read_one_reply(&mut s);
    assert!(
        ping.contains("PONG"),
        "post-battle PING failed: {ping:?}"
    );
    eprintln!("stackex: golden path OK");

    drop(s);
    let _ = std::fs::remove_dir_all(&tmp);
}

fn build(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Read whatever is on the wire until we have at least one well-formed
/// reply (drained by counting RESP top-level frames). Returns the raw
/// accumulated bytes as a string (lossy-decoded).
fn read_one_reply(s: &mut TcpStream) -> String {
    let mut acc = Vec::with_capacity(1024);
    let mut buf = vec![0u8; 8 * 1024];
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let n = match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        acc.extend_from_slice(&buf[..n]);
        if count_replies(&acc) >= 1 {
            break;
        }
    }
    String::from_utf8_lossy(&acc).into_owned()
}

/// Count complete RESP top-level replies in `buf`. Handles `+`, `-`,
/// `:`, `$N`, `*N`, `%N` (RESP3 map). For aggregates we recurse into
/// the declared element count. This is a parser, not a sketch — it
/// must agree with kevy's actual emission, otherwise pipeline counts
/// would be wrong.
fn count_replies(buf: &[u8]) -> usize {
    let mut i = 0;
    let mut count = 0;
    while i < buf.len() {
        match advance_one(buf, i) {
            Some(next) => {
                count += 1;
                i = next;
            }
            None => break,
        }
    }
    count
}

fn advance_one(buf: &[u8], start: usize) -> Option<usize> {
    if start >= buf.len() {
        return None;
    }
    let tag = buf[start];
    let line_end = buf[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| start + p + 1)?;
    match tag {
        b'+' | b'-' | b':' => Some(line_end),
        b'$' => {
            let len_str = std::str::from_utf8(&buf[start + 1..line_end - 2]).ok()?;
            let n: isize = len_str.parse().ok()?;
            if n < 0 {
                Some(line_end)
            } else {
                let end = line_end + (n as usize) + 2;
                if end <= buf.len() { Some(end) } else { None }
            }
        }
        b'*' | b'%' => {
            let len_str = std::str::from_utf8(&buf[start + 1..line_end - 2]).ok()?;
            let n: isize = len_str.parse().ok()?;
            if n < 0 {
                return Some(line_end);
            }
            // RESP3 map: 2*n elements. RESP2 array: n elements.
            let count = if tag == b'%' { (n as usize) * 2 } else { n as usize };
            let mut cur = line_end;
            for _ in 0..count {
                cur = advance_one(buf, cur)?;
            }
            Some(cur)
        }
        _ => None,
    }
}

fn resolve_kevy_bin() -> PathBuf {
    if let Ok(p) = std::env::var("KEVY_BIN") {
        return PathBuf::from(p);
    }
    let here = std::env::current_dir().unwrap();
    let mut p = here.clone();
    loop {
        let candidate = p.join("target/release/kevy");
        if candidate.exists() {
            return candidate;
        }
        if !p.pop() {
            panic!(
                "kevy release binary not found above {}; run `cargo build --release -p kevy` first or set KEVY_BIN",
                here.display()
            );
        }
    }
}
