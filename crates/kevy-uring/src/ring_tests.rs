//! Integration-style tests for [`IoUring`] (split out of `ring.rs` for
//! file-size hygiene). Driven against a live kernel ring whenever io_uring
//! is available — falls back to a SKIP message under sandboxes that block
//! `io_uring_setup` (e.g. Docker's default seccomp profile).

use crate::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Bind a loopback listener and return it alongside its port. Used by tests
/// that need an fd for `prep_accept` — the std listener owns the fd and
/// closes it on drop.
fn loopback_listener() -> (TcpListener, u16) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    (l, port)
}

/// io_uring may be unavailable under a restricted seccomp profile (Docker's
/// default blocks io_uring_setup → EPERM/ENOSYS). Run with
/// `--security-opt seccomp=unconfined` so these actually exercise the engine;
/// they skip (rather than fail) where the syscall is denied.
fn ring_or_skip(entries: u32) -> Option<IoUring> {
    match IoUring::new(entries) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("SKIP: io_uring unavailable ({e})");
            None
        }
    }
}

#[test]
fn nop_round_trips() {
    let Some(mut ring) = ring_or_skip(8) else {
        return;
    };
    assert!(ring.prep_nop(0x1234));
    assert_eq!(ring.submit_and_wait(1).unwrap(), 1);
    let mut got = None;
    let n = ring.for_each_completion(|c| got = Some(c));
    assert_eq!(n, 1);
    let c = got.expect("one completion");
    assert_eq!(c.user_data, 0x1234);
    assert_eq!(c.res, 0); // NOP succeeds with res 0
}

#[test]
fn timeout_fires_after_elapsing() {
    let Some(mut ring) = ring_or_skip(8) else {
        return;
    };
    const ETIME: i32 = 62;
    let ts = KernelTimespec::from_millis(5);
    // SAFETY: `ts` outlives the wait below (reaped before it drops).
    assert!(unsafe { ring.prep_timeout(&ts, 0x71) });
    let start = std::time::Instant::now();
    ring.submit_and_wait(1).unwrap();
    let mut got = None;
    let n = ring.for_each_completion(|c| got = Some(c));
    assert_eq!(n, 1);
    let c = got.expect("one completion");
    assert_eq!(c.user_data, 0x71);
    assert_eq!(c.res, -ETIME); // plain expiry, not cancellation
    assert!(start.elapsed() >= std::time::Duration::from_millis(5));
}

#[test]
fn timeout_bounds_a_wait_alongside_a_pending_read() {
    let Some(mut ring) = ring_or_skip(8) else {
        return;
    };
    // A pipe with no writer activity: the read SQE stays pending forever, so
    // only the timeout can satisfy submit_and_wait(1) — exactly the parked-
    // reactor shape (waker read + bounded timeout).
    let (reader, _writer) = std::io::pipe().unwrap();
    let mut buf = [0u8; 8];
    // SAFETY: buf and ts outlive the wait; both completions reaped below.
    assert!(unsafe { ring.prep_read(reader.as_raw_fd(), buf.as_mut_ptr(), 8, 0x72) });
    let ts = KernelTimespec::from_millis(5);
    assert!(unsafe { ring.prep_timeout(&ts, 0x73) });
    ring.submit_and_wait(1).unwrap();
    let mut datas = Vec::new();
    ring.for_each_completion(|c| datas.push(c.user_data));
    assert!(datas.contains(&0x73), "timeout CQE should arrive, got {datas:?}");
    assert!(!datas.contains(&0x72), "read must still be pending");
}

#[test]
fn reads_a_file() {
    let Some(mut ring) = ring_or_skip(8) else {
        return;
    };
    let path = std::env::temp_dir().join(format!("kevy-uring-{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"hello io_uring").unwrap();
        f.sync_all().unwrap();
    }
    let file = std::fs::File::open(&path).unwrap();
    let mut buf = [0u8; 64];
    unsafe {
        assert!(ring.prep_read(file.as_raw_fd(), buf.as_mut_ptr(), buf.len() as u32, 0xABCD));
    }
    assert_eq!(ring.submit_and_wait(1).unwrap(), 1);
    let mut got = None;
    ring.for_each_completion(|c| got = Some(c));
    let c = got.expect("one completion");
    assert_eq!(c.user_data, 0xABCD);
    assert_eq!(c.res, 14, "should read 14 bytes");
    assert_eq!(&buf[..14], b"hello io_uring");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cancel_unknown_target_reports_enoent() {
    // ASYNC_CANCEL targeting a user_data with no in-flight SQE: kernel
    // emits a single CQE for the cancel SQE itself with res = -ENOENT.
    let Some(mut ring) = ring_or_skip(8) else {
        return;
    };
    const CANCEL_TAG: u64 = 0xCA10;
    const PHANTOM_TARGET: u64 = 0xDEAD;
    assert!(ring.prep_cancel(PHANTOM_TARGET, CANCEL_TAG));
    assert_eq!(ring.submit_and_wait(1).unwrap(), 1);
    let mut got = None;
    ring.for_each_completion(|c| got = Some(c));
    let c = got.expect("cancel completion");
    assert_eq!(c.user_data, CANCEL_TAG);
    // Linux's ENOENT is 2 → -ENOENT == -2.
    assert_eq!(c.res, -2, "cancel of unknown target should return -ENOENT");
}

#[test]
fn cancel_an_in_flight_timeout() {
    // Arm a 60-second timeout, then cancel it. Expect two CQEs:
    // - cancel's own (res = 0 on successful match)
    // - target timeout's (res = -ECANCELED)
    let Some(mut ring) = ring_or_skip(8) else {
        return;
    };
    const TIMEOUT_TAG: u64 = 0x71;
    const CANCEL_TAG: u64 = 0xCA11;
    let ts = KernelTimespec::from_millis(60_000);
    // SAFETY: `&ts` outlives the SQE — both go out of scope at end of fn,
    // and we drain both CQEs before then via submit_and_wait + reap.
    assert!(unsafe { ring.prep_timeout(&ts, TIMEOUT_TAG) });
    ring.submit_and_wait(0).unwrap();
    assert!(ring.prep_cancel(TIMEOUT_TAG, CANCEL_TAG));
    // submit_and_wait's return is "SQEs submitted this call" (1 — just the
    // cancel; the timeout was submitted in the previous call). wait_nr=2
    // makes the kernel block until ≥ 2 CQEs are queued.
    ring.submit_and_wait(2).unwrap();
    let mut cancel_res: Option<i32> = None;
    let mut target_res: Option<i32> = None;
    ring.for_each_completion(|c| match c.user_data {
        CANCEL_TAG => cancel_res = Some(c.res),
        TIMEOUT_TAG => target_res = Some(c.res),
        _ => {}
    });
    // Cancel SQE itself: res 0 means matched and cancelled.
    assert_eq!(cancel_res, Some(0), "cancel should report success");
    // Target SQE: res -ECANCELED (-125) means cancellation succeeded.
    // Linux timeout can ALSO report -ETIME (-62) if it expired
    // concurrently, but at 60s that's structurally impossible here.
    assert_eq!(target_res, Some(-125), "target should report -ECANCELED");
}

#[test]
fn batched_nops() {
    // Submit a full batch, reap them all — exercises ring wrap + counts.
    let Some(mut ring) = ring_or_skip(8) else {
        return;
    };
    for i in 0..8u64 {
        assert!(ring.prep_nop(i));
    }
    assert!(!ring.prep_nop(99), "9th submission should report SQ full");
    assert_eq!(ring.submit_and_wait(8).unwrap(), 8);
    let mut seen = 0u64;
    let n = ring.for_each_completion(|c| seen |= 1 << c.user_data);
    assert_eq!(n, 8);
    assert_eq!(seen, 0xFF, "all 8 user_data tags present");
}

#[test]
fn accepts_a_connection() {
    // io_uring ACCEPT: a pending connection on the listener is accepted and
    // its fd arrives as the completion's `res` (≥ 0).
    let Some(mut ring) = ring_or_skip(8) else {
        return;
    };
    let (listener, port) = loopback_listener();
    // Connect first so the accept can complete immediately from the backlog.
    let _client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();

    assert!(ring.prep_accept(listener.as_raw_fd(), 0xACCE));
    assert_eq!(ring.submit_and_wait(1).unwrap(), 1);
    let mut got = None;
    ring.for_each_completion(|c| got = Some(c));
    let c = got.expect("accept completion");
    assert_eq!(c.user_data, 0xACCE);
    assert!(c.res >= 0, "accepted fd should be >= 0, got {}", c.res);
    // SAFETY: `c.res` is the freshly accepted fd; wrap so drop closes it.
    let _ = unsafe { OwnedFd::from_raw_fd(c.res) };
}

#[test]
fn echo_round_trip_via_io_uring() {
    // Drive a full accept → read → write echo entirely through io_uring —
    // the exact completion loop the Phase-2 reactor will run. A client thread
    // connects, sends, and verifies the echo.
    const ACCEPT: u64 = 1;
    const READ: u64 = 2;
    const WRITE: u64 = 3;

    let Some(mut ring) = ring_or_skip(16) else {
        return;
    };
    let (listener, port) = loopback_listener();

    let client = std::thread::spawn(move || {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping", "client should receive the echo");
    });

    // accept (blocks in the kernel until the client connects)
    assert!(ring.prep_accept(listener.as_raw_fd(), ACCEPT));
    ring.submit_and_wait(1).unwrap();
    let mut conn_fd = -1;
    ring.for_each_completion(|c| {
        if c.user_data == ACCEPT {
            conn_fd = c.res;
        }
    });
    assert!(conn_fd >= 0, "accept failed: {conn_fd}");

    // read the request
    let mut rbuf = [0u8; 64];
    unsafe { assert!(ring.prep_read(conn_fd, rbuf.as_mut_ptr(), rbuf.len() as u32, READ)) };
    ring.submit_and_wait(1).unwrap();
    let mut nread = 0;
    ring.for_each_completion(|c| {
        if c.user_data == READ {
            nread = c.res;
        }
    });
    assert_eq!(nread, 4, "should read 4 bytes");
    assert_eq!(&rbuf[..4], b"ping");

    // write the echo back
    unsafe { assert!(ring.prep_write(conn_fd, rbuf.as_ptr(), 4, WRITE)) };
    ring.submit_and_wait(1).unwrap();
    let mut nwrote = 0;
    ring.for_each_completion(|c| {
        if c.user_data == WRITE {
            nwrote = c.res;
        }
    });
    assert_eq!(nwrote, 4, "should write 4 bytes");

    client.join().unwrap();
    // SAFETY: `conn_fd` is the accepted fd; wrap so drop closes it.
    let _ = unsafe { OwnedFd::from_raw_fd(conn_fd) };
}

#[test]
fn multishot_recv_with_provided_buffers() {
    // One multishot RECV SQE must yield a completion per arrival, each into a
    // kernel-picked provided buffer (bid reported in cqe.flags), staying armed
    // (F_MORE) across recycles — the exact mechanism the reactor relies on.
    const ACCEPT: u64 = 1;
    const RECV: u64 = 2;

    let Some(mut ring) = ring_or_skip(16) else {
        return;
    };
    // Provided-buffer ring may be unsupported on older kernels → skip.
    let (listener, port) = loopback_listener();

    let client = std::thread::spawn(move || {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_nodelay(true).unwrap();
        s.write_all(b"ping").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        s.write_all(b"pong").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
    });

    assert!(ring.prep_accept(listener.as_raw_fd(), ACCEPT));
    ring.submit_and_wait(1).unwrap();
    let mut conn_fd = -1;
    ring.for_each_completion(|c| {
        if c.user_data == ACCEPT {
            conn_fd = c.res;
        }
    });
    assert!(conn_fd >= 0, "accept failed: {conn_fd}");

    let mut pbr = match ring.register_buf_ring(4, 64, 7) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIP: provided buffer ring unavailable ({e})");
            let _ = unsafe { OwnedFd::from_raw_fd(conn_fd) };
            client.join().unwrap();
            return;
        }
    };
    assert!(ring.prep_recv_multishot(conn_fd, pbr.group(), RECV));

    // First arrival.
    ring.submit_and_wait(1).unwrap();
    let mut c1 = None;
    ring.for_each_completion(|c| {
        if c.user_data == RECV {
            c1 = Some(c);
        }
    });
    let c1 = c1.expect("first recv completion");
    assert!(c1.res > 0, "recv res should be >0, got {}", c1.res);
    let bid1 = c1.buffer_id().expect("a provided buffer was used");
    assert_eq!(pbr.bytes(bid1, c1.res as usize), b"ping");
    assert!(c1.has_more(), "multishot recv stays armed (F_MORE)");
    pbr.recycle(bid1);

    // Second arrival — WITHOUT re-submitting the recv SQE (multishot).
    ring.submit_and_wait(1).unwrap();
    let mut c2 = None;
    ring.for_each_completion(|c| {
        if c.user_data == RECV {
            c2 = Some(c);
        }
    });
    let c2 = c2.expect("second recv completion from the same SQE");
    let bid2 = c2.buffer_id().expect("a provided buffer was used");
    assert_eq!(pbr.bytes(bid2, c2.res as usize), b"pong");
    pbr.recycle(bid2);

    client.join().unwrap();
    // SAFETY: `conn_fd` is the accepted fd; wrap so drop closes it.
    let _ = unsafe { OwnedFd::from_raw_fd(conn_fd) };
}
