//! Detection suite: data written, SAVEd, and reloaded by a fresh runtime (same
//! shard count) survives a "restart". Each shard persists its own store.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kevy_testnet::free_port;

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
    assert_eq!(&buf, expected, "expected {:?}", String::from_utf8_lossy(expected));
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
        let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(nshards))
            .bind([127, 0, 0, 1], port)
            .shards(nshards)
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
    let dir = kevy_tmpdir::unique_dir("persist");
    let nshards = 4;
    let port = free_port();

    // First run: write 100 keys and SAVE.
    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..100u32 {
            c.write_all(&req(&[b"SET", format!("k{i}").as_bytes(), format!("v{i}").as_bytes()]))
                .unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        c.write_all(&req(&[b"SAVE"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
    });

    // Per-shard snapshot files should now exist.
    let dumps = (0..nshards).filter(|i| dir.join(format!("dump-{i}.rdb")).exists()).count();
    assert!(dumps > 0, "no snapshot files were written");

    // Second run: a fresh runtime over the same dir must see the data.
    let port2 = free_port();
    with_runtime(port2, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..100u32 {
            c.write_all(&req(&[b"GET", format!("k{i}").as_bytes()])).unwrap();
            let want = format!("v{i}");
            read_reply(&mut c, format!("${}\r\n{}\r\n", want.len(), want).as_bytes());
        }
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bgrewriteaof_shrinks_log_and_preserves_data() {
    let dir = kevy_tmpdir::unique_dir("bgrewrite");
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
                .map(|s| std::fs::metadata(dir.join(format!("aof-{s}.aof"))).map_or(0, |m| m.len()))
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
            c.write_all(&req(&[b"GET", format!("k{i}").as_bytes()])).unwrap();
            let want = format!("v{i}-r49");
            read_reply(&mut c, format!("${}\r\n{}\r\n", want.len(), want).as_bytes());
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
    let dir = kevy_tmpdir::unique_dir("truncated");
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
            c.write_all(&req(&[b"GET", format!("survivor{i}").as_bytes()])).unwrap();
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
    let dir = kevy_tmpdir::unique_dir("aof");
    let nshards = 4;
    let port = free_port();

    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..100u32 {
            c.write_all(&req(&[b"SET", format!("a{i}").as_bytes(), format!("b{i}").as_bytes()]))
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
            c.write_all(&req(&[b"GET", format!("a{i}").as_bytes()])).unwrap();
            let want = format!("b{i}");
            read_reply(&mut c, format!("${}\r\n{}\r\n", want.len(), want).as_bytes());
        }
        // counter must be exactly 5 (replayed once each, not doubled).
        c.write_all(&req(&[b"GET", b"counter"])).unwrap();
        read_reply(&mut c, b"$1\r\n5\r\n");
    });

    let _ = std::fs::remove_dir_all(&dir);
}

/// A blocking pop that finds data pops at once, and that pop is in the
/// AOF: after a restart the popped elements stay popped. Renames replay
/// too. One shard, so the pop runs on the connection's own shard; four,
/// so some keys live elsewhere.
#[test]
fn blocking_pops_and_renames_survive_restart_via_aof() {
    for nshards in [1, 4] {
        let dir = kevy_tmpdir::unique_dir("aof-blocking-pop");
        with_runtime(free_port(), &dir, nshards, |p| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
            c.write_all(&req(&[b"RPUSH", b"q", b"a", b"b", b"c", b"d"])).unwrap();
            read_reply(&mut c, b":4\r\n");
            c.write_all(&req(&[b"BLPOP", b"q", b"0"])).unwrap();
            read_reply(&mut c, b"*2\r\n$1\r\nq\r\n$1\r\na\r\n");
            c.write_all(&req(&[b"BRPOP", b"q", b"0"])).unwrap();
            read_reply(&mut c, b"*2\r\n$1\r\nq\r\n$1\r\nd\r\n");
            c.write_all(&req(&[b"SET", b"{r}1", b"v"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
            c.write_all(&req(&[b"RENAME", b"{r}1", b"{r}2"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
            c.write_all(&req(&[b"RENAMENX", b"{r}2", b"{r}3"])).unwrap();
            read_reply(&mut c, b":1\r\n");
        });
        with_runtime(free_port(), &dir, nshards, |p| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
            c.write_all(&req(&[b"LRANGE", b"q", b"0", b"-1"])).unwrap();
            read_reply(&mut c, b"*2\r\n$1\r\nb\r\n$1\r\nc\r\n");
            c.write_all(&req(&[b"GET", b"{r}3"])).unwrap();
            read_reply(&mut c, b"$1\r\nv\r\n");
            c.write_all(&req(&[b"EXISTS", b"{r}1", b"{r}2"])).unwrap();
            read_reply(&mut c, b":0\r\n");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A blocking pop that parks and is then served by a push: the pop is in
/// the AOF like any other. With four shards some keys live on another
/// shard than the waiting connection, so both the in-shard and the
/// cross-shard serve run.
#[test]
fn parked_blocking_pops_survive_restart_via_aof() {
    let keys: Vec<Vec<u8>> = (0..6).map(|i| format!("w{i}").into_bytes()).collect();
    for nshards in [1, 4] {
        let dir = kevy_tmpdir::unique_dir("aof-parked-pop");
        with_runtime(free_port(), &dir, nshards, |p| {
            let patience = Some(std::time::Duration::from_secs(10));
            let mut pusher = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
            pusher.set_read_timeout(patience).unwrap();
            for key in &keys {
                let mut waiter = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
                waiter.set_read_timeout(patience).unwrap();
                waiter.write_all(&req(&[b"BLPOP", key, b"0"])).unwrap();
                wait_for("the waiter to park", || {
                    pusher.write_all(&req(&[b"INFO", b"clients"])).unwrap();
                    let mut buf = [0u8; 4096];
                    let n = pusher.read(&mut buf).unwrap();
                    String::from_utf8_lossy(&buf[..n]).contains("blocked_clients:1")
                });
                pusher.write_all(&req(&[b"RPUSH", key, b"x", b"y"])).unwrap();
                read_reply(&mut pusher, b":2\r\n");
                let mut want = format!("*2\r\n${}\r\n", key.len()).into_bytes();
                want.extend_from_slice(key);
                want.extend_from_slice(b"\r\n$1\r\nx\r\n");
                read_reply(&mut waiter, &want);
            }
        });
        with_runtime(free_port(), &dir, nshards, |p| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
            c.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
            for key in &keys {
                c.write_all(&req(&[b"LRANGE", key, b"0", b"-1"])).unwrap();
                read_reply(&mut c, b"*1\r\n$1\r\ny\r\n");
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn restart_tolerates_corrupt_snapshot() {
    // Coverage: drive the `load_snapshot` Err branch in shard::run (the
    // eprintln path). A corrupt dump-0.rdb should produce a startup warning
    // on stderr but NOT prevent the reactor from coming up; subsequent
    // writes go through normally.
    let dir = kevy_tmpdir::unique_dir("corrupt-snap");

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
    let dir = kevy_tmpdir::unique_dir("auto-rewrite");
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
        read_reply(&mut c, b"$32\r\nrevision-number-padding-00000799\r\n");
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn auto_aof_rewrite_respects_pct_zero_disable() {
    // `auto_aof_rewrite_pct = 0` disables the tick-driven rewrite —
    // even after crossing the min_size floor, the AOF must keep
    // accumulating until a client calls BGREWRITEAOF explicitly.
    let dir = kevy_tmpdir::unique_dir("auto-rewrite-off");
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

            // Generous deadline: the appends sit in the AOF BufWriter until a
            // background flush tick lands them on disk, and a loaded CI
            // runner has missed a 1 s window (observed: still 9 bytes on the
            // macOS runner). The waiter returns the moment the floor is
            // reached, so the slack costs nothing on a healthy run.
            let pre = wait_for_size_at_least_heartbeat(&aof_path, &mut c, 16 * 1024, 5_000);
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
            let post = std::fs::metadata(&aof_path).map_or(0, |m| m.len());
            assert!(post >= pre, "auto-rewrite fired despite pct=0: {post} vs {pre} pre");
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
        let sz = std::fs::metadata(path).map_or(0, |m| m.len());
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
        let sz = std::fs::metadata(path).map_or(0, |m| m.len());
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

/// Incident regression: a relative TTL must survive a restart at its
/// *original* wall-clock deadline, not be reset to a fresh full duration.
/// Before the fix, AOF replay re-anchored `PEXPIRE` to restart-time, so PTTL
/// after restart read back the full 100 s; the fix logs an absolute
/// `PEXPIREAT`, so the ~3 s spent down is correctly subtracted.
#[test]
fn relative_ttl_survives_restart_at_original_deadline() {
    let dir = kevy_tmpdir::unique_dir("ttl-restart");
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

// ───────────── stream consumer groups survive restart ─────────────

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
        b"XREADGROUP",
        b"GROUP",
        b"g",
        b"c1",
        b"COUNT",
        b"2",
        b"STREAMS",
        b"st",
        b">",
    ]))
    .unwrap();
    read_reply(
        c,
        format!("*1\r\n*2\r\n$2\r\nst\r\n*2\r\n{}{}", entry("1-1"), entry("2-1")).as_bytes(),
    );
    c.write_all(&req(&[b"XREADGROUP", b"GROUP", b"g", b"c2", b"STREAMS", b"st", b">"])).unwrap();
    read_reply(c, format!("*1\r\n*2\r\n$2\r\nst\r\n*1\r\n{}", entry("3-1")).as_bytes());
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
/// (3 pending, c1=2), the rewrite drops it (2 pending, c1=1) — a
/// deliberate trade-off (the rewrite re-serializes only live entries).
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
    c.write_all(&req(&[b"XREADGROUP", b"GROUP", b"g", b"c1", b"STREAMS", b"st", b"0"])).unwrap();
    read_reply(c, b"*1\r\n*2\r\n$2\r\nst\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n");
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
    let dir = kevy_tmpdir::unique_dir("groups-aof");
    let port = free_port();
    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        build_grouped_stream(&mut c);
        c.write_all(&req(&[b"BGREWRITEAOF"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        // Background rewrite: wait for the compacted file to swap in
        // before stopping the runtime. Discriminator: the rewritten
        // image recreates each group with `XGROUP CREATE … MKSTREAM`,
        // which this test never issues, and carries no XDEL frame — so the
        // one's PRESENCE and the other's ABSENCE prove the swap landed.
        // (The log itself records a group read as XCLAIM frames, so
        // XCLAIM tells nothing; and appends reach the disk after the
        // replies, so a check on the log alone matches too early.)
        wait_for("rewritten AOF to swap in", || {
            std::fs::read(dir.join("aof-0.aof")).is_ok_and(|now| {
                now.windows(8).any(|w| w == b"MKSTREAM")
                    && !now.windows(10).any(|w| w == b"$4\r\nXDEL\r\n")
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
    let dir = kevy_tmpdir::unique_dir("groups-save");
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
    let dir = kevy_tmpdir::unique_dir("bgsave");
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
    let dir = kevy_tmpdir::unique_dir("info-persist");
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
            let len: usize = String::from_utf8_lossy(&hdr[1..hdr.len() - 2]).parse().unwrap();
            let mut body = vec![0u8; len + 2];
            c.read_exact(&mut body).unwrap();
            String::from_utf8_lossy(&body).into_owned()
        };
        wait_for("INFO to report the completed rewrite", || {
            let s = info(&mut c);
            s.contains("aof_rewrites_total:1") && s.contains("aof_rewrite_in_progress:0")
        });
        // v4.1-V6 (smix): the on-disk format is a readable state — an
        // AOF-enabled 4.x store writes v2 (and after a rewrite it
        // could not be anything else).
        wait_for("INFO to report the AOF format", || info(&mut c).contains("aof_format:v2"));
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// `SAVE` was migrated from inline
/// `save_snapshot` (synchronous, held the reactor for the disk write)
/// to [`Shard::start_bg_save`] (per-shard `PersistWorker` does the
/// disk work; reactor returns `+OK` as soon as the COW
/// `SnapshotView` is frozen). This test exercises the unblock by
/// populating a keyspace large enough that a synchronous save would
/// take noticeable wall time, then proving GET/SET continue to be
/// served within milliseconds of submitting `SAVE` — long before the
/// snapshot file lands on disk.
#[test]
fn save_does_not_block_reactor_for_disk_write() {
    let dir = kevy_tmpdir::unique_dir("save-async");
    let nshards = 4;
    let port = free_port();
    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        // 20k 256-byte values ≈ 5 MB — enough that the per-shard
        // RDB write takes >>1 ms even on NVMe, so a synchronous
        // save would be observable as a GET stall.
        let big = vec![b'x'; 256];
        for i in 0..20_000u32 {
            let mut argv = req(&[b"SET", format!("k{i}").as_bytes(), &big]);
            argv.extend_from_slice(&[]);
            c.write_all(&argv).unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        // SAVE is async — should return `+OK` near-instantly.
        let save_t0 = std::time::Instant::now();
        c.write_all(&req(&[b"SAVE"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        let save_reply_us = save_t0.elapsed().as_micros();
        // A synchronous 5 MB × 4-shard write is typically 5-30 ms.
        // The async path frees the reactor in <1 ms (the COW view
        // freeze + mpsc send to the worker). Be generous to soak
        // up CI noise (loaded macs in particular).
        assert!(
            save_reply_us < 50_000,
            "SAVE +OK took {save_reply_us} µs — expected <50 ms (\
             reactor blocked? sync save regression?)"
        );
        // Reactor is still serving — issue a GET on a key the
        // pre-SAVE writes inserted. With sync SAVE this would be
        // queued behind the disk write; async SAVE serves immediately.
        let get_t0 = std::time::Instant::now();
        c.write_all(&req(&[b"GET", b"k1"])).unwrap();
        let mut prefix = [0u8; 7];
        c.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"$256\r\nx");
        // Drain the rest of the value (255 x's + \r\n).
        let mut rest = vec![0u8; 255 + 2];
        c.read_exact(&mut rest).unwrap();
        let get_us = get_t0.elapsed().as_micros();
        assert!(
            get_us < 50_000,
            "GET after SAVE took {get_us} µs — expected <50 ms (reactor blocked?)"
        );
        // Wait for the bg save to land all shards' dump files
        // (shutdown drain would do this anyway, but make it explicit
        // for the assertion below).
        wait_for("background SAVE to land all shard dumps", || {
            (0..nshards).all(|s| dir.join(format!("dump-{s}.rdb")).exists())
        });
    });
    // Restart over the same dir: data must be there (i.e. the async
    // SAVE actually finished durably before runtime exit, via the
    // shutdown drain).
    let port2 = free_port();
    with_runtime(port2, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"DBSIZE"])).unwrap();
        let mut buf = [0u8; 16];
        let n = c.read(&mut buf).unwrap();
        let reply = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            reply.starts_with(":20000\r\n"),
            "DBSIZE after restart = {reply:?} (expected :20000\\r\\n — \
             async SAVE failed to land before shutdown?)"
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Shutdown drain: a `SAVE` submitted just
/// before `stop=true` must still land its `dump-{i}.rdb` rename + AOF
/// reset, because the client got `+OK` on the COW view freeze and
/// would otherwise be lied to. `with_runtime`'s normal `stop` →
/// `handle.join()` is sufficient because both reactor loops
/// (`run` / `run_uring`) call `drain_persist_on_shutdown` before
/// returning.
#[test]
fn save_at_shutdown_drains_to_disk() {
    let dir = kevy_tmpdir::unique_dir("save-shutdown");
    let nshards = 4;
    let port = free_port();
    with_runtime(port, &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        // Large enough that the worker is still mid-write when
        // `with_runtime` flips `stop=true` and joins. The drain
        // has to actually block on the worker.
        let big = vec![b'y'; 1024];
        for i in 0..5_000u32 {
            c.write_all(&req(&[b"SET", format!("k{i}").as_bytes(), &big])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        c.write_all(&req(&[b"SAVE"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        // Don't `wait_for` here — leave the runtime to drop while
        // the bg save is (most likely) still in flight, exercising
        // the shutdown drain path.
    });
    // Every shard's snapshot must exist post-shutdown — the drain
    // forced the bg-save rename to complete before runtime exit.
    let dumps_after = (0..nshards).filter(|i| dir.join(format!("dump-{i}.rdb")).exists()).count();
    assert_eq!(
        dumps_after, nshards,
        "shutdown drain did not flush all shards' snapshots: \
         only {dumps_after}/{nshards} dump-N.rdb files exist"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Probe written while auditing a consumer's TTL-inflation report: a
/// RELATIVE ttl frame (SETEX, SET … EX, GETEX … EX) must not re-anchor on replay — the AOF
/// carries whatever the write path logged, and if that is the verb
/// itself, every restart hands the key its full TTL back. The rewrite
/// path already normalizes to absolute PEXPIREAT; this pins the
/// pre-rewrite window.
#[test]
fn relative_ttl_frames_do_not_reanchor_on_replay() {
    let dir = kevy_tmpdir::unique_dir("ttl-reanchor");
    let port = free_port();
    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"SETEX", b"grey", b"100", b"v"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"SET", b"grey3", b"v", b"EX", b"100"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"SET", b"grey4", b"v"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"GETEX", b"grey4", b"EX", b"100"])).unwrap();
        read_reply(&mut c, b"$1\r\nv\r\n");
        c.write_all(&req(&[b"EXPIRE", b"grey2", b"100"])).unwrap(); // no such key: 0
        let mut buf = [0u8; 64];
        let _ = c.read(&mut buf).unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let port = free_port();
    with_runtime(port, &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for key in [&b"grey"[..], b"grey3", b"grey4"] {
            c.write_all(&req(&[b"PTTL", key])).unwrap();
            let mut buf = [0u8; 64];
            let n = c.read(&mut buf).unwrap();
            let s = String::from_utf8_lossy(&buf[..n]);
            let ttl: i64 = s.trim_start_matches(':').trim().parse().expect("integer PTTL");
            let key = String::from_utf8_lossy(key);
            assert!(ttl > 0, "{key} survived the restart: {s}");
            assert!(
                ttl <= 100_000 - 2_000,
                "{key}: TTL re-anchored on replay: read {ttl}ms of an original 100000ms \
                 after >=2.5s elapsed — the AOF frame must carry an absolute deadline"
            );
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Writes that arrive after a BGSAVE has finished and swapped in the reset
/// AOF land in that new log and survive a restart.
#[test]
fn writes_after_the_bgsave_swap_survive_a_restart() {
    let dir = kevy_tmpdir::unique_dir("bgsave-after");
    let nshards = 4;
    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for i in 0..20u32 {
            c.write_all(&req(&[b"SET", format!("pre{i}").as_bytes(), b"v"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        c.write_all(&req(&[b"BGSAVE"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        wait_for("the reset logs to be swapped in", || {
            (0..nshards).all(|s| {
                dir.join(format!("dump-{s}.rdb")).exists()
                    && std::fs::read(dir.join(format!("aof-{s}.aof")))
                        .is_ok_and(|b| !b.windows(3).any(|w| w == b"pre"))
            })
        });
        for i in 0..40u32 {
            c.write_all(&req(&[b"SET", format!("post{i}").as_bytes(), b"v"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        wait_for("the later writes to reach the new logs while running", || {
            let posts = |s: usize| {
                std::fs::read(dir.join(format!("aof-{s}.aof")))
                    .map_or(0, |b| b.windows(4).filter(|w| *w == b"post").count())
            };
            (0..nshards).map(posts).sum::<usize>() == 40
        });
    });
    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        c.write_all(&req(&[b"DBSIZE"])).unwrap();
        read_reply(&mut c, b":60\r\n");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Writes that arrive after a BGREWRITEAOF has swapped in the compacted
/// log land in that new log and survive a restart.
#[test]
fn writes_after_the_rewrite_swap_survive_a_restart() {
    let dir = kevy_tmpdir::unique_dir("rewrite-after");
    let nshards = 4;
    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        for rev in 0..50u32 {
            for i in 0..20u32 {
                c.write_all(&req(&[
                    b"SET",
                    format!("pre{i}").as_bytes(),
                    format!("r{rev}").as_bytes(),
                ]))
                .unwrap();
                read_reply(&mut c, b"+OK\r\n");
            }
        }
        c.write_all(&req(&[b"BGREWRITEAOF"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        wait_for("every compacted log to be swapped in", || {
            (0..nshards).all(|s| {
                std::fs::read(dir.join(format!("aof-{s}.aof")))
                    .is_ok_and(|b| !b.windows(3).any(|w| w == b"r48"))
            })
        });
        for i in 0..40u32 {
            c.write_all(&req(&[b"SET", format!("post{i}").as_bytes(), b"v"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
        }
        wait_for("the later writes to reach the new logs while running", || {
            let posts = |s: usize| {
                std::fs::read(dir.join(format!("aof-{s}.aof")))
                    .map_or(0, |b| b.windows(4).filter(|w| *w == b"post").count())
            };
            (0..nshards).map(posts).sum::<usize>() == 40
        });
    });
    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        c.write_all(&req(&[b"DBSIZE"])).unwrap();
        read_reply(&mut c, b":60\r\n");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// A conditional `HEXPIRE` keeps its absolute deadlines across a restart:
/// the field it moved does not count its TTL from replay time, and the
/// field its condition refused keeps the deadline it already had.
#[test]
fn conditional_field_ttl_keeps_its_deadlines_across_replay() {
    let dir = kevy_tmpdir::unique_dir("field-ttl-reanchor");
    let ints = |c: &mut std::net::TcpStream| -> Vec<i64> {
        let mut buf = [0u8; 128];
        let n = c.read(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n])
            .split("\r\n")
            .filter_map(|l| l.strip_prefix(':').and_then(|v| v.parse().ok()))
            .collect()
    };
    with_runtime(free_port(), &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"HSET", b"h", b"f", b"v", b"g", b"w"])).unwrap();
        assert_eq!(ints(&mut c), [2]);
        c.write_all(&req(&[b"HEXPIRE", b"h", b"100", b"FIELDS", b"1", b"g"])).unwrap();
        assert_eq!(ints(&mut c), [1]);
        c.write_all(&req(&[b"HEXPIRE", b"h", b"200", b"NX", b"FIELDS", b"2", b"f", b"g"])).unwrap();
        assert_eq!(ints(&mut c), [1, 0], "NX moves f and refuses g");
    });
    std::thread::sleep(std::time::Duration::from_millis(2500));
    with_runtime(free_port(), &dir, 1, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"HPTTL", b"h", b"FIELDS", b"2", b"f", b"g"])).unwrap();
        let ttl = ints(&mut c);
        assert!(ttl[0] > 0 && ttl[0] <= 200_000 - 2_000, "f re-anchored on replay: {ttl:?}");
        assert!(ttl[1] > 0 && ttl[1] <= 100_000 - 2_000, "g took a deadline it refused: {ttl:?}");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// `MSET` and a same-shard `RENAME` must survive a restart.
///
/// Both are served to clients by the routing layer, and the op records
/// its effect into the AOF **using the same verb** — but replay goes
/// through the local dispatcher, where `MSET` answered an arity error
/// and `RENAME` was not implemented at all. The record was written and
/// could not be replayed, so the write was acknowledged, readable, and
/// gone after the next start. Measured before the fix, with
/// `appendfsync always`: all four `MSET` keys absent, and `RENAME`
/// *reverted* — the source key alive again, the destination missing.
#[test]
fn mset_and_rename_survive_a_restart() {
    let dir = kevy_tmpdir::unique_dir("persist-replayverbs");
    // One shard, so RENAME takes the same-shard atomic op (the
    // cross-shard two-step is a separate record path).
    let nshards = 1;

    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"CONFIG", b"SET", b"appendfsync", b"always"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"MSET", b"m:1", b"a", b"m:2", b"b"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"SET", b"src", b"v"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"RENAME", b"src", b"dst"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
    });

    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"GET", b"m:1"])).unwrap();
        read_reply(&mut c, b"$1\r\na\r\n");
        c.write_all(&req(&[b"GET", b"m:2"])).unwrap();
        read_reply(&mut c, b"$1\r\nb\r\n");
        c.write_all(&req(&[b"GET", b"dst"])).unwrap();
        read_reply(&mut c, b"$1\r\nv\r\n");
        // …and the rename really moved it rather than copying.
        c.write_all(&req(&[b"GET", b"src"])).unwrap();
        read_reply(&mut c, b"$-1\r\n");
    });

    let _ = std::fs::remove_dir_all(&dir);
}

/// A cross-shard `RENAME` must survive a restart — every value type,
/// its TTL, and the refusal branch.
///
/// The two halves land on different shards, and neither used to write a
/// record at all: `Op::RenameTake` removed the source and `Op::RenamePut`
/// placed the value, both silently, so a restart reverted the whole
/// rename (source alive again, destination missing). The destination now
/// records the value through the rewrite serializer, and the source
/// records its delete **after** the put commits — never at take time,
/// because a refused `RENAMENX` rolls the value back and an early
/// delete would outlive that rollback as a lie.
#[test]
fn cross_shard_rename_survives_a_restart() {
    let dir = kevy_tmpdir::unique_dir("persist-xrename");
    let nshards = 4;

    // Pick pairs that genuinely straddle two shards — a same-shard pair
    // would exercise the atomic op and prove nothing about this path.
    let cross = |a: &[u8], b: &[u8]| {
        kevy_rt::shard_of_key(a, nshards, kevy_persist::Routing::KevyHash)
            != kevy_rt::shard_of_key(b, nshards, kevy_persist::Routing::KevyHash)
    };
    assert!(cross(b"src", b"dst"), "test fixture must be cross-shard");
    assert!(cross(b"h:src", b"h:dst"), "hash fixture must be cross-shard");
    assert!(cross(b"keep", b"taken"), "refusal fixture must be cross-shard");

    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"CONFIG", b"SET", b"appendfsync", b"always"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"SET", b"src", b"v"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"EXPIRE", b"src", b"1000"])).unwrap();
        read_reply(&mut c, b":1\r\n");
        c.write_all(&req(&[b"HSET", b"h:src", b"f", b"1"])).unwrap();
        read_reply(&mut c, b":1\r\n");
        c.write_all(&req(&[b"RENAME", b"src", b"dst"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"RENAME", b"h:src", b"h:dst"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        // The refusal branch: dst exists, so the value goes home.
        c.write_all(&req(&[b"SET", b"keep", b"mine"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"SET", b"taken", b"theirs"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        c.write_all(&req(&[b"RENAMENX", b"keep", b"taken"])).unwrap();
        read_reply(&mut c, b":0\r\n");
    });

    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&req(&[b"GET", b"dst"])).unwrap();
        read_reply(&mut c, b"$1\r\nv\r\n");
        c.write_all(&req(&[b"GET", b"src"])).unwrap();
        read_reply(&mut c, b"$-1\r\n");
        // The TTL rode along rather than being dropped or reset.
        c.write_all(&req(&[b"TTL", b"dst"])).unwrap();
        let mut buf = [0u8; 32];
        let n = c.read(&mut buf).unwrap();
        let ttl: i64 = String::from_utf8_lossy(&buf[1..n - 2]).parse().unwrap();
        assert!((900..=1000).contains(&ttl), "TTL must survive the move: {ttl}");
        c.write_all(&req(&[b"HGET", b"h:dst", b"f"])).unwrap();
        read_reply(&mut c, b"$1\r\n1\r\n");
        c.write_all(&req(&[b"EXISTS", b"h:src"])).unwrap();
        read_reply(&mut c, b":0\r\n");
        // Refused rename: both keys exactly as they were.
        c.write_all(&req(&[b"GET", b"keep"])).unwrap();
        read_reply(&mut c, b"$4\r\nmine\r\n");
        c.write_all(&req(&[b"GET", b"taken"])).unwrap();
        read_reply(&mut c, b"$6\r\ntheirs\r\n");
    });

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every value type — and its TTL — must round-trip through a snapshot.
///
/// The snapshot is the *second* writer of the same data (the AOF is the
/// first), with its own format and its own loader. The AOF pair turned
/// out to have three holes where a value was written in a form its
/// reader could not restore, so the sibling pair deserves the same
/// question asked of it rather than assumed. Today's answer is clean;
/// this keeps it that way. Existing coverage was strings
/// (`data_survives_restart_via_save`) and stream groups only.
#[test]
fn every_value_type_round_trips_through_a_snapshot() {
    let dir = kevy_tmpdir::unique_dir("persist-snaptypes");
    let nshards = 2;

    with_runtime_configured(
        free_port(),
        &dir,
        nshards,
        |rt| rt.with_aof(false),
        |p| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
            c.write_all(&req(&[b"SET", b"str", b"v"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
            c.write_all(&req(&[b"EXPIRE", b"str", b"500"])).unwrap();
            read_reply(&mut c, b":1\r\n");
            c.write_all(&req(&[b"HSET", b"h", b"f1", b"1", b"f2", b"2"])).unwrap();
            read_reply(&mut c, b":2\r\n");
            c.write_all(&req(&[b"RPUSH", b"l", b"a", b"b", b"c"])).unwrap();
            read_reply(&mut c, b":3\r\n");
            c.write_all(&req(&[b"SADD", b"s", b"m1", b"m2"])).unwrap();
            read_reply(&mut c, b":2\r\n");
            c.write_all(&req(&[b"ZADD", b"z", b"2.5", b"zn"])).unwrap();
            read_reply(&mut c, b":1\r\n");
            c.write_all(&req(&[b"HSET", b"hx", b"g", b"1"])).unwrap();
            read_reply(&mut c, b":1\r\n");
            c.write_all(&req(&[b"HEXPIRE", b"hx", b"400", b"FIELDS", b"1", b"g"])).unwrap();
            read_reply(&mut c, b"*1\r\n:1\r\n");
            c.write_all(&req(&[b"XADD", b"strm", b"1-1", b"f", b"v"])).unwrap();
            read_reply(&mut c, b"$3\r\n1-1\r\n");
            c.write_all(&req(&[b"SAVE"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
        },
    );

    // Prove the snapshot is what carries this: a dump per shard, and no
    // AOF anywhere. Without this the test could pass vacuously on an AOF
    // that was never disabled.
    let dumps = (0..nshards).filter(|i| dir.join(format!("dump-{i}.rdb")).exists()).count();
    assert!(dumps > 0, "no snapshot was written — nothing to round-trip");
    let aofs = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".aof"))
        .count();
    assert_eq!(aofs, 0, "an AOF exists, so the snapshot is not what is under test");

    with_runtime_configured(
        free_port(),
        &dir,
        nshards,
        |rt| rt.with_aof(false),
        |p| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
            c.write_all(&req(&[b"GET", b"str"])).unwrap();
            read_reply(&mut c, b"$1\r\nv\r\n");
            c.write_all(&req(&[b"HGET", b"h", b"f2"])).unwrap();
            read_reply(&mut c, b"$1\r\n2\r\n");
            c.write_all(&req(&[b"LRANGE", b"l", b"0", b"-1"])).unwrap();
            read_reply(&mut c, b"*3\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n");
            c.write_all(&req(&[b"SISMEMBER", b"s", b"m2"])).unwrap();
            read_reply(&mut c, b":1\r\n");
            c.write_all(&req(&[b"ZSCORE", b"z", b"zn"])).unwrap();
            read_reply(&mut c, b"$3\r\n2.5\r\n");
            c.write_all(&req(&[b"XRANGE", b"strm", b"-", b"+"])).unwrap();
            read_reply(&mut c, b"*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n");
            // The two TTL flavours: key-level and hash-field-level. Both are
            // absolute deadlines, so they come back a little smaller.
            for (probe, floor) in [
                (req(&[b"TTL", b"str"]), 400i64),
                (req(&[b"HTTL", b"hx", b"FIELDS", b"1", b"g"]), 300i64),
            ] {
                c.write_all(&probe).unwrap();
                let mut buf = [0u8; 64];
                let n = c.read(&mut buf).unwrap();
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let secs: i64 = text
                    .rsplit(':')
                    .next()
                    .and_then(|t| t.trim_end_matches("\r\n").parse().ok())
                    .unwrap_or(-1);
                assert!(secs > floor, "TTL must survive the snapshot, got {text:?}");
            }
        },
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Read one whole RESP reply, raw.
fn read_resp(s: &mut std::net::TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    read_resp_into(s, &mut out);
    out
}

fn read_resp_into(s: &mut std::net::TcpStream, out: &mut Vec<u8>) {
    let start = out.len();
    let mut byte = [0u8; 1];
    loop {
        s.read_exact(&mut byte).unwrap();
        out.push(byte[0]);
        if out.len() - start >= 3 && out.ends_with(b"\r\n") {
            break;
        }
    }
    let line = std::str::from_utf8(&out[start + 1..out.len() - 2]).unwrap();
    let n: i64 = line.parse().unwrap_or(0);
    match out[start] {
        b'$' if n >= 0 => {
            let mut body = vec![0u8; n as usize + 2];
            s.read_exact(&mut body).unwrap();
            out.extend_from_slice(&body);
        }
        b'*' | b'%' | b'~' => {
            let items = if out[start] == b'%' { 2 * n } else { n };
            for _ in 0..items.max(0) {
                read_resp_into(s, out);
            }
        }
        _ => {}
    }
}

/// Every key the pairing tests write, and the command that reads it back.
fn pairing_reads() -> Vec<Vec<Vec<u8>>> {
    let mut reads = Vec::new();
    for i in 0..8 {
        let k = |p: &str| format!("{p}{i}").into_bytes();
        reads.push(vec![b"LRANGE".to_vec(), k("l"), b"0".to_vec(), b"-1".to_vec()]);
        reads.push(vec![b"XRANGE".to_vec(), k("x"), b"-".to_vec(), b"+".to_vec()]);
        reads.push(vec![b"GET".to_vec(), k("s")]);
        reads.push(vec![b"HGET".to_vec(), k("h"), b"f".to_vec()]);
    }
    reads
}

/// Non-idempotent writes, tagged `tag`, on keys spread over every shard.
/// Each `junk` key is overwritten, so a rewritten log no longer holds
/// `<tag>-old`.
fn pairing_writes(c: &mut std::net::TcpStream, tag: &str) {
    for i in 0..8 {
        let k = |p: &str| format!("{p}{i}").into_bytes();
        let v = |s: &str| format!("{tag}-{s}").into_bytes();
        let cmds: [Vec<Vec<u8>>; 6] = [
            vec![b"RPUSH".to_vec(), k("l"), v("a"), v("b"), v("c")],
            vec![b"XADD".to_vec(), k("x"), b"*".to_vec(), b"f".to_vec(), v("1")],
            vec![b"APPEND".to_vec(), k("s"), v("x")],
            vec![b"HINCRBY".to_vec(), k("h"), b"f".to_vec(), b"5".to_vec()],
            vec![b"SET".to_vec(), k("junk"), v("old")],
            vec![b"SET".to_vec(), k("junk"), v("new")],
        ];
        for cmd in cmds {
            let parts: Vec<&[u8]> = cmd.iter().map(Vec::as_slice).collect();
            c.write_all(&req(&parts)).unwrap();
            let reply = read_resp(c);
            assert_ne!(reply.first(), Some(&b'-'), "{}", String::from_utf8_lossy(&reply));
        }
    }
}

fn read_all(c: &mut std::net::TcpStream) -> Vec<Vec<u8>> {
    pairing_reads()
        .iter()
        .map(|cmd| {
            let parts: Vec<&[u8]> = cmd.iter().map(Vec::as_slice).collect();
            c.write_all(&req(&parts)).unwrap();
            read_resp(c)
        })
        .collect()
}

fn aof_holds(dir: &std::path::Path, s: usize, needle: &[u8]) -> bool {
    std::fs::read(dir.join(format!("aof-{s}.aof")))
        .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle))
}

/// Restart `dir` and compare every key with what the client read before.
fn assert_restores(dir: &std::path::Path, nshards: usize, before: &[Vec<u8>]) {
    with_runtime(free_port(), dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let after = read_all(&mut c);
        for ((cmd, b), a) in pairing_reads().iter().zip(before).zip(&after) {
            assert_eq!(
                String::from_utf8_lossy(a),
                String::from_utf8_lossy(b),
                "{} {} after the restart",
                String::from_utf8_lossy(&cmd[0]),
                String::from_utf8_lossy(&cmd[1]),
            );
        }
    });
}

/// BGSAVE, then BGREWRITEAOF: the rewritten log is a complete image, and a
/// restart must not load the snapshot under it. Before the fix every write
/// from before the rewrite was applied twice (a list read `a b c a b c`).
#[test]
fn a_rewrite_after_a_bgsave_restores_each_write_once() {
    let dir = kevy_tmpdir::unique_dir("pair-save-rewrite");
    let nshards = 2;
    let mut before = Vec::new();
    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        pairing_writes(&mut c, "pre");
        c.write_all(&req(&[b"BGSAVE"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        wait_for("every snapshot and log reset", || {
            (0..nshards)
                .all(|s| dir.join(format!("dump-{s}.rdb")).exists() && !aof_holds(&dir, s, b"pre-"))
        });
        pairing_writes(&mut c, "mid");
        // a shard skips a rewrite while a background job is in flight
        wait_for("every rewritten log", || {
            c.write_all(&req(&[b"BGREWRITEAOF"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
            std::thread::sleep(std::time::Duration::from_millis(50));
            (0..nshards).all(|s| aof_holds(&dir, s, b"pre-") && !aof_holds(&dir, s, b"mid-old"))
        });
        pairing_writes(&mut c, "post");
        before = read_all(&mut c);
    });
    assert!(String::from_utf8_lossy(&before[0]).contains("mid-a"), "the lists were written");
    assert_restores(&dir, nshards, &before);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other order: a BGSAVE after a rewrite starts a log that continues
/// the snapshot, and a restart restores the snapshot and that log over it.
#[test]
fn a_bgsave_after_a_rewrite_restores_each_write_once() {
    let dir = kevy_tmpdir::unique_dir("pair-rewrite-save");
    let nshards = 2;
    let mut before = Vec::new();
    with_runtime(free_port(), &dir, nshards, |p| {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", p)).unwrap();
        pairing_writes(&mut c, "pre");
        c.write_all(&req(&[b"BGREWRITEAOF"])).unwrap();
        read_reply(&mut c, b"+OK\r\n");
        wait_for("every rewritten log", || {
            (0..nshards).all(|s| aof_holds(&dir, s, b"pre-new") && !aof_holds(&dir, s, b"pre-old"))
        });
        pairing_writes(&mut c, "mid");
        // a shard skips a BGSAVE while its rewrite's teardown is in flight
        wait_for("every snapshot and log reset", || {
            c.write_all(&req(&[b"BGSAVE"])).unwrap();
            read_reply(&mut c, b"+OK\r\n");
            std::thread::sleep(std::time::Duration::from_millis(50));
            (0..nshards)
                .all(|s| dir.join(format!("dump-{s}.rdb")).exists() && !aof_holds(&dir, s, b"mid-"))
        });
        pairing_writes(&mut c, "post");
        before = read_all(&mut c);
    });
    assert_restores(&dir, nshards, &before);
    let _ = std::fs::remove_dir_all(&dir);
}
