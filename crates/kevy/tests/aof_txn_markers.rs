//! Transaction markers belong to transactions — and to nothing else.
//!
//! The release-matrix diskgate caught every single-command reactor batch
//! paying a KEVYTXNBEGIN/COMMIT record pair (+65 B/op on plain SETs —
//! AOF 178 bytes/op against a 106 baseline). The fix split the window:
//! reactor batches keep only the group-fsync half (a pipelined batch is
//! not a transaction), while EXEC brackets its queued commands itself.
//! These tests pin both sides of that split at the AOF byte level.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn free_port() -> u16 {
    use std::sync::atomic::AtomicU16;
    static NEXT: AtomicU16 = AtomicU16::new(26_500);
    loop {
        let p = NEXT.fetch_add(1, Ordering::Relaxed);
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("kevy-txnmark-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One shard, fsync=always: every EXEC stays local (the marker bracket
/// is deterministic) and every append is on disk before the reply.
fn spawn_server(port: u16, dir: std::path::PathBuf) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = Arc::clone(&stop);
    std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
            .bind([127, 0, 0, 1], port)
            .shards(1)
            .with_data_dir(dir)
            .with_aof(true)
            .with_appendfsync(kevy_rt::Fsync::Always);
        let _ = rt.run(stop_t);
    });
    for _ in 0..400 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return stop;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("server on {port} never came up");
}

fn cmd(conn: &mut TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    let mut req = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        req.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        req.extend_from_slice(p);
        req.extend_from_slice(b"\r\n");
    }
    conn.write_all(&req).unwrap();
    let mut buf = [0u8; 4096];
    let n = conn.read(&mut buf).unwrap();
    buf[..n].to_vec()
}

fn count_subslice(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

fn aof_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut all = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.file_name().to_string_lossy().ends_with(".aof") {
            all.extend(std::fs::read(e.path()).unwrap());
        }
    }
    all
}

#[test]
fn single_commands_carry_no_markers_and_exec_is_bracketed() {
    let port = free_port();
    let dir = tmp_dir("main");
    let stop = spawn_server(port, dir.clone());
    let mut conn = TcpStream::connect(("127.0.0.1", port)).unwrap();

    // Phase 1: plain single-command batches — the dominant client shape.
    for i in 0..20 {
        let key = format!("plain:{i}");
        cmd(&mut conn, &[b"SET", key.as_bytes(), b"value-0123456789"]);
    }
    let aof = aof_bytes(&dir);
    assert_eq!(
        count_subslice(&aof, b"KEVYTXNBEGIN"),
        0,
        "single-command batches must not pay the transaction-marker pair"
    );

    // Phase 2: MULTI/EXEC — the real atomic unit gets the bracket.
    cmd(&mut conn, &[b"MULTI"]);
    cmd(&mut conn, &[b"SET", b"tx:a", b"1"]);
    cmd(&mut conn, &[b"SET", b"tx:b", b"2"]);
    cmd(&mut conn, &[b"SET", b"tx:c", b"3"]);
    let exec = cmd(&mut conn, &[b"EXEC"]);
    assert!(exec.starts_with(b"*3"), "EXEC should answer 3 replies: {exec:?}");
    let aof = aof_bytes(&dir);
    assert_eq!(count_subslice(&aof, b"KEVYTXNBEGIN"), 1, "EXEC opens exactly one marker window");
    assert_eq!(count_subslice(&aof, b"KEVYTXNCOMMIT"), 1, "and closes it");
    // The three writes sit between the markers.
    let begin = aof.windows(12).position(|w| w == b"KEVYTXNBEGIN").unwrap();
    let commit = aof.windows(13).position(|w| w == b"KEVYTXNCOMMIT").unwrap();
    for k in [&b"tx:a"[..], b"tx:b", b"tx:c"] {
        let at = aof.windows(k.len()).position(|w| w == k).unwrap();
        assert!(begin < at && at < commit, "{} outside the marker window", String::from_utf8_lossy(k));
    }

    // Phase 3: the whole log replays — reopen and read both phases back.
    stop.store(true, Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_millis(300));
    drop(conn);
    let port2 = free_port();
    let stop2 = spawn_server(port2, dir.clone());
    let mut conn2 = TcpStream::connect(("127.0.0.1", port2)).unwrap();
    let got = cmd(&mut conn2, &[b"GET", b"tx:b"]);
    assert!(got.starts_with(b"$1\r\n2"), "EXEC write must replay: {got:?}");
    let got = cmd(&mut conn2, &[b"GET", b"plain:7"]);
    assert!(got.starts_with(b"$16"), "plain write must replay: {got:?}");
    stop2.store(true, Ordering::Relaxed);
    let _ = std::fs::remove_dir_all(&dir);
}
