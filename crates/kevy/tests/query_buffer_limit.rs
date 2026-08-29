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

    // Before anything is measured: prove every connection to this port
    // reaches the server this test started, and only it.
    //
    // This rules ONE hypothesis out, and it is worth saying which,
    // because the first version of this comment asserted it as the
    // cause and was wrong. `kevy-testnet::free_port` hands out ports
    // from a block this process holds exclusively — an anchor listener
    // sits on the block's base for the process's lifetime, so no other
    // test binary can draw from it, and each port is confirmed bindable
    // at the moment it is returned. Another test's server taking this
    // port is not a thing that can happen, and claiming it was is the
    // kind of tidy story that stops anyone looking further.
    //
    // What IS true is that this cell has failed three times in CI with
    // "the connection was STILL open 30s later", and that sentence is
    // also what a server talking to the wrong client would produce. The
    // check below costs a millisecond and removes that reading for
    // good, so a fourth failure means what it says.
    //
    // The cause of the three is NOT established. What would establish
    // it is knowing whether the server DECIDED to close — the
    // enforcement path prints "closing conn N: query buffer exceeded"
    // and calls `uring_mark_closing`, and there is no counter to ask.
    // A decision that never reached the client is a different defect
    // from a cap that was never noticed, and this test cannot tell them
    // apart from where it stands.
    {
        let marker = format!("qbuf-marker-{port}");
        let mut w = TcpStream::connect(("127.0.0.1", port)).unwrap();
        w.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let set = format!(
            "*3\r\n$3\r\nSET\r\n${}\r\n{marker}\r\n$2\r\nok\r\n",
            marker.len()
        );
        w.write_all(set.as_bytes()).unwrap();
        let mut buf = [0u8; 64];
        let n = w.read(&mut buf).unwrap_or(0);
        assert_eq!(&buf[..n], b"+OK\r\n", "the marker write was refused");

        let get = format!("*2\r\n$3\r\nGET\r\n${}\r\n{marker}\r\n", marker.len());
        for attempt in 1..=6 {
            let mut r = TcpStream::connect(("127.0.0.1", port)).unwrap();
            r.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            r.write_all(get.as_bytes()).unwrap();
            let mut b = [0u8; 64];
            let n = r.read(&mut b).unwrap_or(0);
            assert_eq!(
                &b[..n],
                b"$2\r\nok\r\n",
                "connection {attempt} to port {port} did not find this test's own \
                 marker — the port is shared with another server, so nothing \
                 measured below would have been about the server this test started"
            );
        }
    }

    // A syntactically valid frame that never completes, streamed as
    // MANY small args (not one big bulk): a multibulk declaring a huge
    // arg count, then `$3\r\nabc\r\n` bulks forever. Small bulks
    // accumulate in the connection's input buffer on BOTH reactors
    // (the big-single-bulk shape would divert to the io_uring
    // kernel-direct path and never touch the query buffer — the reason
    // an earlier version of this test passed on epoll but not on the
    // io_uring CI runner).
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.write_all(b"*1000000\r\n").unwrap();
    let mut junk = Vec::new();
    for _ in 0..256 {
        junk.extend_from_slice(b"$3\r\nabc\r\n"); // 256 small args per write
    }

    // Phase 1 — cross the cap, then STOP writing.
    //
    // What the guard promises is that accumulated unparsed input past the
    // cap closes the connection; the server judges that on the recv it is
    // already handling. It does not promise to have finished closing by any
    // particular point on the CLIENT's wall clock. The earlier shape of this
    // cell conflated the two — it kept writing while it looked, for ~3.8 s
    // in total, and called anything slower a failure. That is a window, not
    // the contract, and it is what failed twice in CI with the server's own
    // "closing conn N: query buffer exceeded" sitting in the log.
    const CAP: usize = 4096; // KEVY_DEBUG_INPUT_LIMIT, set above
    let mut written = 10usize; // the multibulk header already sent
    let mut writes = 0usize;
    let mut closed_as: Option<String> = None;
    while written <= CAP * 2 {
        if let Err(e) = c.write_all(&junk) {
            // Closed while we were still feeding it — contract met, early,
            // and there is nothing left to wait for.
            closed_as = Some(format!("write failed: {:?}", e.kind()));
            break;
        }
        written += junk.len();
        writes += 1;
    }

    // Phase 2 — wait for the close without sending another byte.
    //
    // The budget is deliberately loose. Reaping is bounded in REACTOR
    // iterations, not in wall time: `uring_mark_closing` pushes the conn onto
    // the closing ready-set, a non-empty set holds the reactor out of its park
    // rung (`reap_pending` in the idle ladder), and the reap runs on every
    // sixteenth iteration — microseconds on an idle core, but only once that
    // thread is scheduled at all. On a loaded runner the only honest bound is
    // "eventually". A server that never closes still fails here, which is the
    // defect this cell exists to catch, and the observed latency rides along
    // in the message either way.
    const BUDGET: Duration = Duration::from_secs(30);
    c.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let started = std::time::Instant::now();
    let mut polls = 0usize;
    while closed_as.is_none() && started.elapsed() < BUDGET {
        polls += 1;
        let mut probe = [0u8; 8];
        match c.read(&mut probe) {
            Ok(0) => closed_as = Some(String::from("read returned 0 (EOF)")),
            Ok(n) => panic!("no reply should exist before the frame completes; read {n} bytes"),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => closed_as = Some(format!("read failed: {:?}", e.kind())),
        }
    }

    assert!(
        closed_as.is_some(),
        "a multibulk of small args put {written} bytes past the {CAP}-byte cap in \
         {writes} writes, and the connection was STILL open {:?} later — {polls} \
         read polls, not one of which saw a close",
        started.elapsed()
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
