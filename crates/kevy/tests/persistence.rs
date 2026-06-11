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

/// Poll `cond` up to ~10 s (BGREWRITEAOF/BGSAVE are background since the
/// COW-serialization change: +OK returns at the view freeze, the swap lands
/// on a later tick). Panics with `what` on timeout.
fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if cond() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// Run a runtime on `port` in `dir` with `nshards`, hand it to `body`, then stop.
fn with_runtime(port: u16, dir: &std::path::Path, nshards: usize, body: impl FnOnce(u16)) {
    with_runtime_configured(port, dir, nshards, |rt| rt, body);
}

/// Variant that lets the caller customise the `Runtime` (e.g. enable
/// auto-rewrite) before it starts. The closure receives the builder and
/// returns the modified builder; `with_data_dir` and `KevyCommands` are
/// applied first.
fn with_runtime_configured<F>(
    port: u16,
    dir: &std::path::Path,
    nshards: usize,
    configure: F,
    body: impl FnOnce(u16),
) where
    F: FnOnce(kevy_rt::Runtime<kevy::KevyCommands>) -> kevy_rt::Runtime<kevy::KevyCommands>
        + Send
        + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let dir = dir.to_path_buf();
    let handle = std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
            .with_data_dir(dir);
        let rt = configure(rt);
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

        // Background rewrite: the compacted file swaps in on a later tick.
        let sum_aof = || -> u64 {
            (0..nshards)
                .map(|s| {
                    std::fs::metadata(dir.join(format!("aof-{s}.aof")))
                        .map(|m| m.len())
                        .unwrap_or(0)
                })
                .sum()
        };
        // 40 keys × 1 SET per key, summed across shards, fits well under
        // the size of 2000 raw SETs we would otherwise carry forward.
        // ~30-byte average per SET ⇒ post-rewrite ≤ ~2 KB total.
        wait_for("rewritten AOF to swap in", || sum_aof() < 10_000);
        post_size = sum_aof();
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
        // Read every reply before exiting so we know the shard processed
        // all 5 commands; without this, racing the runtime shutdown can
        // leave INCRs unapplied on a fast Linux host (a flake the Mac
        // happens to dodge).
        for i in 1..=5u32 {
            c.write_all(&req(&[b"INCR", b"counter"])).unwrap();
            let want = format!(":{i}\r\n");
            read_reply(&mut c, want.as_bytes());
        }
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

#[test]
fn restart_tolerates_corrupt_snapshot() {
    // Coverage: drive the `load_snapshot` Err branch in shard::run (the
    // eprintln path). A corrupt dump-0.rdb should produce a startup warning
    // on stderr but NOT prevent the reactor from coming up; subsequent
    // writes go through normally.
    let dir = std::env::temp_dir().join(format!(
        "kevy-corrupt-snap-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // Plant a non-snapshot file at dump-0.rdb. kevy-persist's loader
    // recognises a magic header; arbitrary bytes fail the header check.
    std::fs::write(dir.join("dump-0.rdb"), b"NOT A REAL KEVY SNAPSHOT").unwrap();

    let port = free_port();
    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"PING"])).unwrap();
        read_reply(&mut c, b"+PONG\r\n");
        c.write_all(&req(&[b"SET", b"after-corrupt", b"ok"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"GET", b"after-corrupt"])).unwrap();
        read_reply(&mut c, b"$2\r\nok\r\n");
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn auto_aof_rewrite_fires_when_threshold_crossed() {
    // The active-tick path (`maybe_auto_rewrite_aof`) runs an inline
    // BGREWRITEAOF whenever the live AOF has grown by ≥ pct % over the
    // size at the previous rewrite AND exceeds `min_size` bytes. This
    // test exercises that path: no client-side BGREWRITEAOF call,
    // SETs alone push the AOF past 50 % growth above a 256-byte floor,
    // and ~250 ms later (a few tick cycles) the shard's tick should
    // have rebuilt the AOF in place. Final size must be ≤ pre-rewrite
    // raw size, and every key still readable across a restart.
    let dir = std::env::temp_dir().join(format!(
        "kevy-auto-rewrite-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let nshards = 1; // single-shard so size_bytes() is a single file
    let port = free_port();
    let aof_path = dir.join("aof-0.aof");

    // 50 % growth over a 16 KiB floor. The floor must exceed the AOF's
    // `BufWriter` capacity (8 KiB) so that by the time the logical
    // `aof.size_bytes()` crosses the floor, the on-disk file has been
    // flushed enough times for `metadata().len()` polling to observe the
    // growth — otherwise the trigger could fire and rewrite before the
    // test ever sees bytes hit disk.
    with_runtime_configured(
        port,
        &dir,
        nshards,
        |rt| rt.with_auto_aof_rewrite(50, 16 * 1024),
        |p| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();

            // 800 SETs of the same key with growing values. Each SET adds
            // a ~60-byte multibulk to the log (logical ≈ 48 KiB), well past
            // the 16 KiB × 1.5 trigger threshold. Post-rewrite the file
            // dumps only the latest SET, so it collapses dramatically.
            for rev in 0..800u32 {
                c.write_all(&req(&[
                    b"SET",
                    b"counter",
                    format!("revision-number-padding-{rev:08}").as_bytes(),
                ]))
                .unwrap();
                read_reply(&mut c, b"+OK\r\n");
            }

            // Wait for the auto-rewrite tick to compact the log. 800 ack'd
            // SETs are ≈ 48 KiB of un-rewritten multibulks, so the only way
            // the on-disk file can drop below 8 KiB is a rewrite that
            // collapsed them to the single latest SET. We assert on that
            // shrink alone — NOT on first observing the pre-rewrite peak,
            // which races the rewrite (it can fire before a poll catches the
            // file large, the original flake). Heartbeat PINGs keep the shard
            // in its busy-poll batch so `tick_check` fires and
            // `maybe_auto_rewrite_aof` runs.
            // Generous timeout: the rewrite is tick-driven, so a heavily
            // loaded CI runner (parallel jobs starving the reactor thread)
            // needs headroom — it WILL fire (threshold is met), just maybe not
            // in 5 s. 20 s tolerates that without making a real break hang long.
            let post = wait_for_size_below_heartbeat(&aof_path, &mut c, 8 * 1024, 20_000);
            assert!(
                post < 8 * 1024,
                "auto AOF rewrite did not fire: {post} bytes still on disk after \
                 800 SETs (un-rewritten would be ≈ 48 KiB)"
            );
        },
    );

    // Restart from the auto-rewritten AOF: the final value must come back.
    let port2 = free_port();
    with_runtime(port2, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"GET", b"counter"])).unwrap();
        read_reply(
            &mut c,
            b"$32\r\nrevision-number-padding-00000799\r\n",
        );
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn auto_aof_rewrite_respects_pct_zero_disable() {
    // `auto_aof_rewrite_pct = 0` disables the tick-driven rewrite —
    // even after crossing the min_size floor, the AOF must keep
    // accumulating until a client calls BGREWRITEAOF explicitly.
    let dir = std::env::temp_dir().join(format!(
        "kevy-auto-rewrite-off-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let nshards = 1;
    let port = free_port();
    let aof_path = dir.join("aof-0.aof");

    with_runtime_configured(
        port,
        &dir,
        nshards,
        // pct=0 disables; the min_size value is irrelevant under that guard.
        |rt| rt.with_auto_aof_rewrite(0, 1024),
        |p| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
            // 800 SETs — same volume + value width as the positive test so
            // the BufWriter flushes and on-disk size is comparable.
            for rev in 0..800u32 {
                c.write_all(&req(&[
                    b"SET",
                    b"k",
                    format!("revision-number-padding-{rev:08}").as_bytes(),
                ]))
                .unwrap();
                read_reply(&mut c, b"+OK\r\n");
            }

            let pre = wait_for_size_at_least_heartbeat(&aof_path, &mut c, 16 * 1024, 1_000);
            assert!(pre >= 16 * 1024, "AOF did not grow: {pre} bytes");

            // Heartbeat across several tick cycles so the shard actually
            // reaches `maybe_auto_rewrite_aof` and exercises the
            // `pct == 0` early-return branch; otherwise the assertion
            // below is vacuously true.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
            while std::time::Instant::now() < deadline {
                c.write_all(&req(&[b"PING"])).unwrap();
                read_reply(&mut c, b"+PONG\r\n");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let post = std::fs::metadata(&aof_path).map(|m| m.len()).unwrap_or(0);
            assert!(
                post >= pre,
                "auto-rewrite fired despite pct=0: {post} vs {pre} pre"
            );
        },
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Send a PING on `c` every iter while waiting for `path` to reach
/// `floor` bytes. The shard's `tick_check` counter only fires the active
/// reaper / auto-rewrite path every 256 loop iters, which under park-
/// mode takes ~13 s. PINGs wake the shard, triggering a busy-poll batch
/// that fires `tick_check` within micros.
fn wait_for_size_at_least_heartbeat(
    path: &std::path::Path,
    c: &mut std::net::TcpStream,
    floor: u64,
    timeout_ms: u64,
) -> u64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let sz = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if sz >= floor || std::time::Instant::now() >= deadline {
            return sz;
        }
        let _ = c.write_all(&req(&[b"PING"]));
        read_reply(c, b"+PONG\r\n");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Heartbeat variant of [`wait_for_size_below`]. See
/// [`wait_for_size_at_least_heartbeat`] for the rationale.
fn wait_for_size_below_heartbeat(
    path: &std::path::Path,
    c: &mut std::net::TcpStream,
    pre: u64,
    timeout_ms: u64,
) -> u64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let sz = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if sz < pre || std::time::Instant::now() >= deadline {
            return sz;
        }
        let _ = c.write_all(&req(&[b"PING"]));
        read_reply(c, b"+PONG\r\n");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Read one RESP integer reply (`:<n>\r\n`) byte-by-byte (no buffering, so
/// later reads on the same stream stay aligned).
fn read_integer(s: &mut std::net::TcpStream) -> i64 {
    let mut byte = [0u8; 1];
    s.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], b':', "expected RESP integer");
    let mut n = Vec::new();
    loop {
        s.read_exact(&mut byte).unwrap();
        if byte[0] == b'\r' {
            s.read_exact(&mut byte).unwrap(); // consume \n
            break;
        }
        n.push(byte[0]);
    }
    String::from_utf8(n).unwrap().parse().unwrap()
}

/// INC-2026-06-09 regression: a relative TTL must survive a restart at its
/// *original* wall-clock deadline, not be reset to a fresh full duration.
/// Before the fix, AOF replay re-anchored `PEXPIRE` to restart-time, so PTTL
/// after restart read back the full 100 s; the fix logs an absolute
/// `PEXPIREAT`, so the ~3 s spent down is correctly subtracted.
#[test]
fn relative_ttl_survives_restart_at_original_deadline() {
    let dir = std::env::temp_dir().join(format!(
        "kevy-ttl-restart-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let nshards = 2;

    let port = free_port();
    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"SET", b"k", b"v"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        // 100 s relative TTL — large enough that it can't actually expire
        // during the test, so any "reset to full" is unambiguous.
        c.write_all(&req(&[b"PEXPIRE", b"k", b"100000"])).unwrap();
        read_reply(&mut c, b":1\r\n");
    });

    // Spend ~3 s "down" between the two runtimes.
    std::thread::sleep(std::time::Duration::from_secs(3));

    let port2 = free_port();
    with_runtime(port2, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"GET", b"k"])).unwrap();
        read_reply(&mut c, b"$1\r\nv\r\n"); // value survived
        c.write_all(&req(&[b"PTTL", b"k"])).unwrap();
        let pttl = read_integer(&mut c);
        // Deadline preserved: ~97 s left. A reset-to-full bug reads ~100 s.
        assert!(
            (0..=98_000).contains(&pttl),
            "PTTL after restart = {pttl} ms; expected the original deadline \
             (~97 s) minus downtime, not a reset to the full 100 s"
        );
        assert!(pttl > 90_000, "PTTL {pttl} ms implausibly low — key nearly gone");
    });

    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────── stream consumer groups survive restart (2026-06-11) ─────────────

/// Drive the grouped-stream fixture over the wire: entries 1-1/2-1/3-1 on
/// `st`, group `g`, consumer c1 holds 1-1+2-1, c2 holds 3-1, then 2-1 is
/// XDEL'd (tombstone PEL row). Also `st2`: deleted-only stream + group g2.
fn build_grouped_stream(c: &mut std::net::TcpStream) {
    let entry = |id: &str| format!("*2\r\n$3\r\n{id}\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n");
    for id in ["1-1", "2-1", "3-1"] {
        c.write_all(&req(&[b"XADD", b"st", id.as_bytes(), b"f", b"v"])).unwrap();
        read_reply(c, format!("$3\r\n{id}\r\n").as_bytes());
    }
    c.write_all(&req(&[b"XGROUP", b"CREATE", b"st", b"g", b"0"])).unwrap();
    read_reply(c, b"+OK\r\n");
    c.write_all(&req(&[
        b"XREADGROUP", b"GROUP", b"g", b"c1", b"COUNT", b"2", b"STREAMS", b"st", b">",
    ]))
    .unwrap();
    read_reply(
        c,
        format!("*1\r\n*2\r\n$2\r\nst\r\n*2\r\n{}{}", entry("1-1"), entry("2-1")).as_bytes(),
    );
    c.write_all(&req(&[b"XREADGROUP", b"GROUP", b"g", b"c2", b"STREAMS", b"st", b">"]))
        .unwrap();
    read_reply(
        c,
        format!("*1\r\n*2\r\n$2\r\nst\r\n*1\r\n{}", entry("3-1")).as_bytes(),
    );
    c.write_all(&req(&[b"XDEL", b"st", b"2-1"])).unwrap();
    read_reply(c, b":1\r\n");
    // st2: deleted-only stream whose last_id must survive, plus a group.
    c.write_all(&req(&[b"XADD", b"st2", b"5-1", b"f", b"v"])).unwrap();
    read_reply(c, b"$3\r\n5-1\r\n");
    c.write_all(&req(&[b"XDEL", b"st2", b"5-1"])).unwrap();
    read_reply(c, b":1\r\n");
    c.write_all(&req(&[b"XGROUP", b"CREATE", b"st2", b"g2", b"5-1"])).unwrap();
    read_reply(c, b"+OK\r\n");
}

/// Post-restart probes shared by the AOF-rewrite and snapshot paths.
/// `pending_total` differs: the snapshot keeps the 2-1 tombstone PEL row
/// (3 pending, c1=2), the rewrite drops it (2 pending, c1=1) — RFC
/// 2026-06-11 trade-off.
fn assert_grouped_stream_restored(c: &mut std::net::TcpStream, tombstone_kept: bool) {
    let (total, c1) = if tombstone_kept { (3, 2) } else { (2, 1) };
    c.write_all(&req(&[b"XPENDING", b"st", b"g"])).unwrap();
    read_reply(
        c,
        format!(
            "*4\r\n:{total}\r\n$3\r\n1-1\r\n$3\r\n3-1\r\n*2\r\n*2\r\n$2\r\nc1\r\n$1\r\n{c1}\r\n*2\r\n$2\r\nc2\r\n$1\r\n1\r\n"
        )
        .as_bytes(),
    );
    // PEL replay: c1 re-reads its own pending entries from 0 — only the
    // still-existing 1-1 comes back (2-1 is deleted in both paths).
    c.write_all(&req(&[b"XREADGROUP", b"GROUP", b"g", b"c1", b"STREAMS", b"st", b"0"]))
        .unwrap();
    read_reply(
        c,
        b"*1\r\n*2\r\n$2\r\nst\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
    );
    // st2: the ID clock survived the restart even though the stream is empty.
    c.write_all(&req(&[b"XADD", b"st2", b"5-1", b"f", b"v"])).unwrap();
    read_reply(
        c,
        b"-ERR The ID specified in XADD is equal or smaller than the target stream top item\r\n",
    );
    c.write_all(&req(&[b"XPENDING", b"st2", b"g2"])).unwrap();
    read_reply(c, b"*4\r\n:0\r\n$-1\r\n$-1\r\n*-1\r\n");
}

#[test]
fn stream_groups_survive_bgrewriteaof_restart() {
    let dir = std::env::temp_dir().join(format!(
        "kevy-groups-aof-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let port = free_port();
    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        build_grouped_stream(&mut c);
        c.write_all(&req(&[b"BGREWRITEAOF"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        // Background rewrite: wait for the compacted file to swap in
        // before stopping the runtime. Discriminator: the raw history
        // contains literal XREADGROUP frames; a rewritten log never does
        // (it reconstructs PELs via XCLAIM). Plain growth (e.g. the
        // begin-rewrite flush) doesn't trip this.
        wait_for("rewritten AOF to swap in", || {
            std::fs::read(dir.join("aof-0.aof")).is_ok_and(|now| {
                !now.is_empty() && !now.windows(10).any(|w| w == b"XREADGROUP")
            })
        });
    });
    let port2 = free_port();
    with_runtime(port2, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        assert_grouped_stream_restored(&mut c, /*tombstone_kept=*/ false);
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stream_groups_survive_save_restart() {
    let dir = std::env::temp_dir().join(format!(
        "kevy-groups-save-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let port = free_port();
    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        build_grouped_stream(&mut c);
        c.write_all(&req(&[b"SAVE"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
    });
    let port2 = free_port();
    with_runtime(port2, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        assert_grouped_stream_restored(&mut c, /*tombstone_kept=*/ true);
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// BGSAVE (COW background save): +OK returns at the view freeze; the
/// snapshot lands on a later tick together with an AOF reset (the log
/// restarts from the collect point). Writes issued after BGSAVE must
/// survive a restart via that reset log, on top of the snapshot.
#[test]
fn bgsave_writes_snapshot_in_background_and_keeps_post_save_writes() {
    let dir = std::env::temp_dir().join(format!(
        "kevy-bgsave-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let nshards = 4;
    let port = free_port();
    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..50u32 {
            c.write_all(&req(&[b"SET", format!("k{i}").as_bytes(), b"v"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        c.write_all(&req(&[b"BGSAVE"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        // Post-collect writes: must survive via the reset AOF.
        for i in 50..60u32 {
            c.write_all(&req(&[b"SET", format!("k{i}").as_bytes(), b"v"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        wait_for("background snapshots to land", || {
            (0..nshards).all(|s| dir.join(format!("dump-{s}.rdb")).exists())
        });
        // The AOF reset swaps in a log that no longer carries the 50
        // pre-collect SETs: k0 appears in the original log (and keeps
        // being appended to it until the swap) but never in the reset
        // one — k0 lives in the snapshot.
        wait_for("aof reset to swap in", || {
            (0..nshards).all(|s| {
                std::fs::read(dir.join(format!("aof-{s}.aof")))
                    .is_ok_and(|b| !b.windows(4).any(|w| w == b"\nk0\r".as_slice()))
            })
        });
    });
    let port2 = free_port();
    with_runtime(port2, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..60u32 {
            c.write_all(&req(&[b"GET", format!("k{i}").as_bytes()])).unwrap();
            read_reply(&mut c, b"$1\r\nv\r\n");
        }
        c.write_all(&req(&[b"DBSIZE"])).unwrap();
        read_reply(&mut c, b":60\r\n");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// `INFO persistence` reflects the answering shard's real background
/// state: rewrites_total increments once a BGREWRITEAOF lands, and
/// in_progress returns to 0 (both refreshed by the reactor tick).
#[test]
fn info_persistence_reports_rewrite_completion() {
    let dir = std::env::temp_dir().join(format!(
        "kevy-info-persist-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let port = free_port();
    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..100u32 {
            c.write_all(&req(&[b"SET", format!("k{i}").as_bytes(), b"v"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        c.write_all(&req(&[b"BGREWRITEAOF"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        let info = |c: &mut std::net::TcpStream| -> String {
            c.write_all(&req(&[b"INFO", b"persistence"])).unwrap();
            // Bulk reply: $<len>\r\n<body>\r\n — read the length line, then body.
            let mut one = [0u8; 1];
            let mut hdr = Vec::new();
            loop {
                c.read_exact(&mut one).unwrap();
                hdr.push(one[0]);
                if hdr.ends_with(b"\r\n") {
                    break;
                }
            }
            let len: usize =
                String::from_utf8_lossy(&hdr[1..hdr.len() - 2]).parse().unwrap();
            let mut body = vec![0u8; len + 2];
            c.read_exact(&mut body).unwrap();
            String::from_utf8_lossy(&body).into_owned()
        };
        wait_for("INFO to report the completed rewrite", || {
            let s = info(&mut c);
            s.contains("aof_rewrites_total:1") && s.contains("aof_rewrite_in_progress:0")
        });
    });
    let _ = std::fs::remove_dir_all(&dir);
}
