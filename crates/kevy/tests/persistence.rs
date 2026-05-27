//! v0.5 detection: data written, SAVEd, and reloaded by a fresh runtime (same
//! shard count) survives a "restart". Each shard persists its own store.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
        "expected {:?}",
        String::from_utf8_lossy(expected)
    );
}

/// Run a runtime on `port` in `dir` with `nshards`, hand it to `body`, then stop.
fn with_runtime(port: u16, dir: &std::path::Path, nshards: usize, body: impl FnOnce(u16)) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let dir = dir.to_path_buf();
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
            .with_data_dir(dir);
        rt.run(stop_t).unwrap();
    });
    let mut up = false;
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(up, "runtime did not start");
    body(port);
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

#[test]
fn data_survives_restart_via_save() {
    let dir = std::env::temp_dir().join(format!(
        "kevy-persist-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let nshards = 4;
    let port = free_port();

    // First run: write 100 keys and SAVE.
    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..100u32 {
            c.write_all(&req(&[
                b"SET",
                format!("k{i}").as_bytes(),
                format!("v{i}").as_bytes(),
            ]))
            .unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        c.write_all(&req(&[b"SAVE"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
    });

    // Per-shard snapshot files should now exist.
    let dumps = (0..nshards)
        .filter(|i| dir.join(format!("dump-{i}.rdb")).exists())
        .count();
    assert!(dumps > 0, "no snapshot files were written");

    // Second run: a fresh runtime over the same dir must see the data.
    let port2 = free_port();
    with_runtime(port2, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..100u32 {
            c.write_all(&req(&[b"GET", format!("k{i}").as_bytes()]))
                .unwrap();
            let want = format!("v{i}");
            read_reply(
                &mut c,
                format!("${}\r\n{}\r\n", want.len(), want).as_bytes(),
            );
        }
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bgrewriteaof_shrinks_log_and_preserves_data() {
    let dir = std::env::temp_dir().join(format!(
        "kevy-bgrewrite-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let nshards = 4;
    let port = free_port();

    let mut post_size: u64 = 0;
    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        // Build up history: each key gets SET 50x. Goal is two-fold:
        //   - overflow the per-shard BufWriter (8 KB default) so disk
        //     content is actually flushed before we sample the file size
        //   - create a large gap (~50× compression) between pre-rewrite
        //     accumulated bytes and post-rewrite compact bytes
        for i in 0..40u32 {
            for rev in 0..50u32 {
                c.write_all(&req(&[
                    b"SET",
                    format!("k{i}").as_bytes(),
                    format!("v{i}-r{rev}").as_bytes(),
                ]))
                .unwrap();
                read_reply(&mut c, b"+OK\r\n");
            }
        }

        c.write_all(&req(&[b"BGREWRITEAOF"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");

        post_size = (0..nshards)
            .map(|s| {
                std::fs::metadata(dir.join(format!("aof-{s}.aof")))
                    .map(|m| m.len())
                    .unwrap_or(0)
            })
            .sum();
        // 40 keys × 1 SET per key, summed across shards, fits well under
        // the size of 2000 raw SETs we would otherwise carry forward.
        // ~30-byte average per SET ⇒ post-rewrite ≤ ~2 KB total.
        assert!(
            post_size < 10_000,
            "rewritten AOF unexpectedly large: {post_size} bytes"
        );
        assert!(post_size > 0, "rewritten AOF should not be empty");
    });

    // Restart from rewritten AOF: every key must come back with its final value.
    let port2 = free_port();
    with_runtime(port2, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..40u32 {
            c.write_all(&req(&[b"GET", format!("k{i}").as_bytes()]))
                .unwrap();
            let want = format!("v{i}-r49");
            read_reply(
                &mut c,
                format!("${}\r\n{}\r\n", want.len(), want).as_bytes(),
            );
        }
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn aof_truncated_tail_is_tolerated_on_restart() {
    // Power-loss / kill -9 simulation: half a write made it to disk before
    // the kernel died. On restart, the prefix must replay cleanly and the
    // partial trailing frame must be silently dropped — never panic, never
    // refuse to start. This is the contract `replay_aof` documents and
    // the active reaper / BGREWRITEAOF + auto-trigger machinery all
    // assume holds.
    let dir = std::env::temp_dir().join(format!(
        "kevy-truncated-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let nshards = 1; // single-shard so we know exactly which AOF to corrupt
    let port = free_port();

    // 1) Write some keys via a real runtime so its AOF is on disk.
    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..20u32 {
            c.write_all(&req(&[
                b"SET",
                format!("survivor{i}").as_bytes(),
                b"v".to_vec().as_slice(),
            ]))
            .unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        // SAVE forces the AOF to flush via the snapshot path (which then
        // truncates the AOF — so we don't SAVE here); instead, BGREWRITEAOF
        // gives us a freshly-flushed AOF whose contents we can corrupt.
        c.write_all(&req(&[b"BGREWRITEAOF"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
    });

    // 2) Corrupt the AOF by appending a half-written frame (truncated bulk).
    //    This simulates a process kill mid-append.
    let aof_path = dir.join("aof-0.aof");
    let mut bytes = std::fs::read(&aof_path).unwrap();
    let prefix_len = bytes.len();
    // Add a malformed multi-bulk that asks for 3 args, gives only header for arg 0.
    bytes.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$5\r\nfoo");
    std::fs::write(&aof_path, &bytes).unwrap();
    let corrupted_len = bytes.len();
    assert!(corrupted_len > prefix_len, "test should have appended garbage");

    // 3) Restart: every clean key from the prefix must survive; corrupt tail
    //    is silently dropped (no panic, no startup failure).
    let port2 = free_port();
    with_runtime(port2, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..20u32 {
            c.write_all(&req(&[b"GET", format!("survivor{i}").as_bytes()]))
                .unwrap();
            read_reply(&mut c, b"$1\r\nv\r\n");
        }
        // The mangled `foo` from the truncated frame must NOT have landed.
        c.write_all(&req(&[b"GET", b"foo"])).unwrap();
        read_reply(&mut c, b"$-1\r\n");
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn data_survives_restart_via_aof_without_save() {
    // No SAVE at all — durability comes purely from the AOF replay on startup.
    let dir = std::env::temp_dir().join(format!(
        "kevy-aof-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let nshards = 4;
    let port = free_port();

    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..100u32 {
            c.write_all(&req(&[
                b"SET",
                format!("a{i}").as_bytes(),
                format!("b{i}").as_bytes(),
            ]))
            .unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        // INCR a few — verifies non-idempotent ops replay exactly once.
        for _ in 0..5 {
            c.write_all(&req(&[b"INCR", b"counter"])).unwrap();
        }
        let mut buf = [0u8; 64];
        let _ = c.read(&mut buf).unwrap();
    });
    // No SAVE: snapshots must NOT exist; AOF must.
    assert!(!dir.join("dump-0.rdb").exists());

    let port2 = free_port();
    with_runtime(port2, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..100u32 {
            c.write_all(&req(&[b"GET", format!("a{i}").as_bytes()]))
                .unwrap();
            let want = format!("b{i}");
            read_reply(
                &mut c,
                format!("${}\r\n{}\r\n", want.len(), want).as_bytes(),
            );
        }
        // counter must be exactly 5 (replayed once each, not doubled).
        c.write_all(&req(&[b"GET", b"counter"])).unwrap();
        read_reply(&mut c, b"$1\r\n5\r\n");
    });

    let _ = std::fs::remove_dir_all(&dir);
}
