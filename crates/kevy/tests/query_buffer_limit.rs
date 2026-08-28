//! The query-buffer guard (V3 storm-protection audit): a client
//! streaming an incomplete-but-valid giant frame must be disconnected
//! once its accumulated unparsed input crosses the cap — never allowed
//! to grow `conn.input` toward OOM. Redis calls the knob
//! client-query-buffer-limit; kevy's cap is a constant with a
//! debug-env override (`KEVY_DEBUG_INPUT_LIMIT`), which is what makes
//! this test possible without streaming a real gigabyte.
//!
//! Own integration binary = own process, so the env var cannot race
//! other tests.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kevy_testnet::free_port;

#[test]
fn a_streaming_giant_frame_is_disconnected_at_the_cap() {
    // The override must be set BEFORE the runtime thread constructs its
    // shards (read once at shard build).
    unsafe { std::env::set_var("KEVY_DEBUG_INPUT_LIMIT", "4096") };
    let port = free_port();
    let dir = std::env::temp_dir().join(format!(
        "kevy-qbuf-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (stop2, dir2) = (stop.clone(), dir.clone());
    let handle = std::thread::spawn(move || {
        kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
            .bind([127, 0, 0, 1], port)
            .shards(1)
            .with_data_dir(dir2)
            .run(stop2)
            .unwrap();
    });
    kevy_testnet::assert_listening(port, "the server under test");

    // A syntactically valid frame that never completes, streamed as
    // MANY small args (not one big bulk): a multibulk declaring a huge
    // arg count, then `$3\r\nabc\r\n` bulks forever. Small bulks
    // accumulate in the connection's input buffer on BOTH reactors
    // (the big-single-bulk shape would divert to the io_uring
    // kernel-direct path and never touch the query buffer — the reason
    // an earlier version of this test passed on epoll but not on the
    // io_uring CI runner).
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    // Short probe timeout: the loop's read is a liveness poll, not a
    // wait-for-reply.
    c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    c.write_all(b"*1000000\r\n").unwrap();
    let mut junk = Vec::new();
    for _ in 0..256 {
        junk.extend_from_slice(b"$3\r\nabc\r\n"); // 256 small args per write
    }
    let started = std::time::Instant::now();
    let mut disconnected = false;
    let mut iters = 0usize;
    let mut written = 10usize; // the multibulk header already sent
    let mut last = String::from("(no probe yet)");
    for _ in 0..64 {
        iters += 1;
        if let Err(e) = c.write_all(&junk) {
            disconnected = true;
            last = format!("write failed: {:?}", e.kind());
            break;
        }
        written += junk.len();
        // Give the reactor a beat to read + judge the accumulation.
        std::thread::sleep(Duration::from_millis(10));
        let mut probe = [0u8; 8];
        match c.read(&mut probe) {
            Ok(0) => {
                disconnected = true;
                last = String::from("read returned 0 (EOF)");
                break;
            }
            Ok(n) => panic!("no reply should exist before the frame completes; read {n} bytes"),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                last = format!("probe timed out ({:?})", e.kind());
            }
            Err(e) => {
                disconnected = true;
                last = format!("read failed: {:?}", e.kind());
                break;
            }
        }
    }
    let looked_for = started.elapsed();

    // The instrument, not the assertion. This cell failed twice in CI with
    // the server's own "closing conn N: query buffer exceeded" in the log
    // and the client seeing nothing, and a bare `assert!(disconnected)`
    // cannot tell "never closed" from "closed later than this loop looked".
    // So when the budget runs out, wait once, long, on the same socket, and
    // put the answer in the failure message. Only runs on the failing path.
    //
    // Worth knowing while reading a future failure: `uring_mark_closing`
    // does not drop the fd — it sets a flag and queues the conn, and
    // `uring_reap_closed` does the drop on every SIXTEENTH reactor
    // iteration. The park's bounding timeout is 50 ms, so sixteen of those
    // is 800 ms worst case, which does not by itself explain a window this
    // size. That gap is what this message exists to close.
    let after = if disconnected {
        String::new()
    } else {
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut probe = [0u8; 8];
        let waited = std::time::Instant::now();
        match c.read(&mut probe) {
            Ok(0) => format!("; then EOF arrived {:?} later", waited.elapsed()),
            Ok(n) => format!("; then {n} bytes arrived, which should not exist"),
            Err(e) => format!("; then still nothing after 5 s more ({:?})", e.kind()),
        }
    };

    assert!(
        disconnected,
        "a multibulk of small args streamed past the 4KB cap without a disconnect \
         — {iters} iterations, {written} bytes written, {looked_for:?} spent looking, \
         last probe: {last}{after}"
    );

    // The server itself is fine: a fresh conn still answers.
    let mut c2 = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c2.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c2.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut buf = [0u8; 16];
    let n = c2.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"+PONG\r\n");

    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(&dir);
}
