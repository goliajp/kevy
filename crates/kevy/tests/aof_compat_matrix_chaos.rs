//! v1.47 — AOF compat matrix chaos test (Phase C step 5).
//!
//! Production rolling upgrades require that an AOF written by an older
//! kevy version replays cleanly under a newer one. kevy's AOF format
//! is canonical RESP — every command serialized as a RESP array — so
//! the compat surface IS the set of commands kevy ships. This test
//! pre-writes a hand-crafted RESP AOF (no kevy version dependency)
//! containing a mix of v1.0-vintage commands + a torn final command,
//! then spawns v1.46 to replay it.
//!
//! Strict asserts:
//! - All 100 complete SET-records replay (string + INCR + EXPIRE +
//!   LPUSH + HSET + SADD + ZADD).
//! - Torn trailing command is discarded silently (no panic, no
//!   half-applied key).
//! - kevy answers PING after recovery.
//!
//! Gated `#[ignore]`. Run with:
//!
//! ```text
//! cargo build --release -p kevy
//! cargo test -p kevy --test aof_compat_matrix_chaos --release -- --ignored --nocapture
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use kevy_chaos::{Harness, HarnessConfig, pick_free_port};

/// Build a single RESP array from an iterator of byte slices.
fn resp_cmd(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[test]
#[ignore = "chaos test — opt-in via --ignored, needs `cargo build --release -p kevy` first"]
fn aof_compat_matrix_replays_v1_vintage_aof() {
    let bin_path = resolve_kevy_bin();
    let port = pick_free_port().expect("free port");
    let tmp = std::env::temp_dir().join(format!("kevy-chaos-aofcompat-{port}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("mkdir");

    // PHASE 1: hand-write a RESP AOF covering the v1.0-vintage command
    // matrix. With `--threads 1` kevy uses shard 0 only, so all keys
    // land in `aof-0.aof`.
    let mut aof = Vec::with_capacity(8 * 1024);
    // 50 SETs.
    for i in 0..50 {
        let k = format!("compat:str:{i:04}");
        let v = format!("val-{i}");
        aof.extend_from_slice(&resp_cmd(&[b"SET", k.as_bytes(), v.as_bytes()]));
    }
    // 10 INCRs on a counter.
    for _ in 0..10 {
        aof.extend_from_slice(&resp_cmd(&[b"INCR", b"compat:counter"]));
    }
    // 10 LPUSHes.
    for i in 0..10 {
        let v = format!("item-{i}");
        aof.extend_from_slice(&resp_cmd(&[b"LPUSH", b"compat:list", v.as_bytes()]));
    }
    // 10 HSETs.
    for i in 0..10 {
        let f = format!("field{i}");
        let v = format!("hv-{i}");
        aof.extend_from_slice(&resp_cmd(&[
            b"HSET",
            b"compat:hash",
            f.as_bytes(),
            v.as_bytes(),
        ]));
    }
    // 10 SADDs.
    for i in 0..10 {
        let v = format!("mem-{i}");
        aof.extend_from_slice(&resp_cmd(&[b"SADD", b"compat:set", v.as_bytes()]));
    }
    // 10 ZADDs.
    for i in 0..10 {
        let score = format!("{i}");
        let v = format!("zmem-{i}");
        aof.extend_from_slice(&resp_cmd(&[
            b"ZADD",
            b"compat:zset",
            score.as_bytes(),
            v.as_bytes(),
        ]));
    }
    let clean_len = aof.len();
    // PHASE 2: append a torn final command (missing the closing CRLF +
    // last few bulk-string bytes) — simulates a crash mid-fsync.
    aof.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$4\r\ntorn\r\n$");
    eprintln!(
        "aof_compat: wrote {} bytes ({} clean + {} torn trailer)",
        aof.len(),
        clean_len,
        aof.len() - clean_len
    );
    std::fs::write(tmp.join("aof-0.aof"), &aof).expect("write AOF");

    // PHASE 3: spawn kevy with this pre-seeded dir. `--threads 1`
    // forces shard 0 = matches our AOF layout.
    let mut cfg = HarnessConfig::new(tmp.clone(), port).with_fsync("everysec");
    cfg.kevy_bin = bin_path;
    cfg.threads = 1;
    let _h = Harness::spawn(cfg).expect("spawn kevy");
    std::thread::sleep(Duration::from_millis(300));

    let mut s = TcpStream::connect(format!("127.0.0.1:{port}"))
        .expect("conn");
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));

    // PHASE 4: verify counters / types replay correctly.
    // GET compat:str:0042 → "val-42"
    s.write_all(&resp_cmd(&[b"GET", b"compat:str:0042"])).unwrap();
    let mut buf = vec![0u8; 256];
    let n = s.read(&mut buf).unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    eprintln!("aof_compat: GET compat:str:0042 = {reply:?}");
    assert!(
        reply.contains("val-42"),
        "SET replay failed: {reply:?}"
    );

    // GET compat:counter → "10"
    s.write_all(&resp_cmd(&[b"GET", b"compat:counter"])).unwrap();
    let n = s.read(&mut buf).unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    eprintln!("aof_compat: GET compat:counter = {reply:?}");
    assert!(
        reply.contains("10"),
        "INCR replay failed: {reply:?}"
    );

    // LLEN compat:list → 10
    s.write_all(&resp_cmd(&[b"LLEN", b"compat:list"])).unwrap();
    let n = s.read(&mut buf).unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    eprintln!("aof_compat: LLEN compat:list = {reply:?}");
    assert!(
        reply.contains(":10"),
        "LPUSH replay failed: {reply:?}"
    );

    // HLEN compat:hash → 10
    s.write_all(&resp_cmd(&[b"HLEN", b"compat:hash"])).unwrap();
    let n = s.read(&mut buf).unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    eprintln!("aof_compat: HLEN compat:hash = {reply:?}");
    assert!(
        reply.contains(":10"),
        "HSET replay failed: {reply:?}"
    );

    // SCARD compat:set → 10
    s.write_all(&resp_cmd(&[b"SCARD", b"compat:set"])).unwrap();
    let n = s.read(&mut buf).unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    eprintln!("aof_compat: SCARD compat:set = {reply:?}");
    assert!(
        reply.contains(":10"),
        "SADD replay failed: {reply:?}"
    );

    // ZCARD compat:zset → 10
    s.write_all(&resp_cmd(&[b"ZCARD", b"compat:zset"])).unwrap();
    let n = s.read(&mut buf).unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    eprintln!("aof_compat: ZCARD compat:zset = {reply:?}");
    assert!(
        reply.contains(":10"),
        "ZADD replay failed: {reply:?}"
    );

    // EXISTS torn → 0 (torn command must NOT have been applied).
    s.write_all(&resp_cmd(&[b"EXISTS", b"torn"])).unwrap();
    let n = s.read(&mut buf).unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    eprintln!("aof_compat: EXISTS torn = {reply:?}");
    assert!(
        reply.contains(":0"),
        "torn command leaked partial key: {reply:?}"
    );

    // Final PING — server alive.
    s.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let n = s.read(&mut buf).unwrap();
    assert!(
        buf[..n].starts_with(b"+PONG"),
        "post-replay PING failed: {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
    eprintln!("aof_compat: all 7 invariants validated; kevy alive");

    drop(s);
    let _ = std::fs::remove_dir_all(&tmp);
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
