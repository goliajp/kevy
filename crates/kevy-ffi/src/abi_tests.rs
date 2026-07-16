//! The ABI exercised from Rust — the reason the crate keeps an rlib
//! target. The C/C++/Go/Bun/Node smokes prove the doors; these prove the
//! stone under plain `cargo test`, where llvm-cov can see it and where an
//! ABI regression fails without a C toolchain in the loop.

use super::*;

fn cmd(db: *mut KevyDb, argv: &[&[u8]]) -> Vec<u8> {
    let ptrs: Vec<*const u8> = argv.iter().map(|a| a.as_ptr()).collect();
    let lens: Vec<usize> = argv.iter().map(|a| a.len()).collect();
    let mut out = KevyBuf::empty();
    let rc = unsafe { kevy_cmd(db, argv.len(), ptrs.as_ptr(), lens.as_ptr(), &raw mut out) };
    assert_eq!(rc, 0, "kevy_cmd misuse");
    take(out)
}

fn take(buf: KevyBuf) -> Vec<u8> {
    let v = if buf.ptr.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) }.to_vec()
    };
    unsafe { kevy_buf_free(buf.ptr, buf.len, buf.cap) };
    v
}

/// Copy the bytes out then free via the SHARED lane (drops the engine Arc).
fn take_shared(buf: KevyBuf) -> Vec<u8> {
    let v = if buf.ptr.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) }.to_vec()
    };
    unsafe { kevy_buf_free_shared(buf.ptr, buf.len, buf.cap) };
    v
}

#[test]
fn shared_get_zero_copy_small_and_bulk_and_plain_lane_unchanged() {
    let db = kevy_open_mem();
    let small = b"tiny"; // inline Value::Str
    let big = vec![0x61u8; 4096]; // > 64 B → Value::ArcBulk (the zero-copy path)
    unsafe {
        assert_eq!(kevy_set(db, b"s".as_ptr(), 1, small.as_ptr(), small.len(), 0), 0);
        assert_eq!(kevy_set(db, b"b".as_ptr(), 1, big.as_ptr(), big.len(), 0), 0);
    }
    // Shared GET returns the exact bytes for both encodings.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_get_shared(db, b"s".as_ptr(), 1, &raw mut out) }, 1);
    assert_eq!(take_shared(out), small);
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_get_shared(db, b"b".as_ptr(), 1, &raw mut out) }, 1);
    assert_eq!(take_shared(out), big);
    // Miss.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_get_shared(db, b"absent".as_ptr(), 6, &raw mut out) }, 0);
    // The bulk value is STILL intact after the shared buffer was freed — the Arc
    // was dropped exactly once, the store's own clone lives on (no UAF/double-free).
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_get(db, b"b".as_ptr(), 1, &raw mut out) }, 1);
    assert_eq!(take(out), big); // plain (Vec) lane byte-unchanged
    // Misuse + null-cap free no-op.
    let mut out = KevyBuf::empty();
    assert!(unsafe { kevy_get_shared(std::ptr::null_mut(), b"s".as_ptr(), 1, &raw mut out) } < 0);
    unsafe { kevy_buf_free_shared(std::ptr::null_mut(), 0, 0) };
    unsafe { kevy_close(db) };
}

#[test]
fn version_and_abi() {
    assert_eq!(kevy_abi(), KEVY_ABI);
    let v = unsafe { std::ffi::CStr::from_ptr(kevy_version()) };
    assert_eq!(v.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn cmd_round_trip_and_protocol_error() {
    let db = kevy_open_mem();
    assert!(!db.is_null());

    assert_eq!(cmd(db, &[b"SET", b"k", b"v"]), b"+OK\r\n");
    assert_eq!(cmd(db, &[b"GET", b"k"]), b"$1\r\nv\r\n");
    // A protocol error is a successful call with a RESP error inside.
    assert_eq!(cmd(db, &[b"NOSUCHVERB"])[0], b'-');

    unsafe { kevy_close(db) };
}

#[test]
fn misuse_is_reported_not_undefined() {
    let mut out = KevyBuf::empty();
    // Null db / zero argc / null out are misuse, not crashes.
    let p: *const u8 = b"X".as_ptr();
    let l = 1usize;
    assert!(unsafe { kevy_cmd(std::ptr::null_mut(), 1, &raw const p, &raw const l, &raw mut out) } < 0);
    let db = kevy_open_mem();
    assert!(unsafe { kevy_cmd(db, 0, &raw const p, &raw const l, &raw mut out) } < 0);
    assert!(unsafe { kevy_cmd(db, 1, &raw const p, &raw const l, std::ptr::null_mut()) } < 0);
    assert!(unsafe { kevy_get(db, std::ptr::null(), 0, &raw mut out) } < 0);
    assert!(unsafe { kevy_set(db, std::ptr::null(), 0, std::ptr::null(), 0, 0) } < 0);
    // Null handles are no-ops, exactly once each.
    unsafe { kevy_close(std::ptr::null_mut()) };
    unsafe { kevy_sub_close(std::ptr::null_mut()) };
    unsafe { kevy_buf_free(std::ptr::null_mut(), 0, 0) };
    unsafe { kevy_close(db) };
}

#[test]
fn scalar_fast_path_hits_misses_and_ttl() {
    let db = kevy_open_mem();
    let k = b"fast";
    assert_eq!(unsafe { kevy_set(db, k.as_ptr(), k.len(), b"v".as_ptr(), 1, 0) }, 0);
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_get(db, k.as_ptr(), k.len(), &raw mut out) }, 1);
    assert_eq!(take(out), b"v");
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_get(db, b"absent".as_ptr(), 6, &raw mut out) }, 0);
    // TTL through the fast path, observed through the verb surface.
    assert_eq!(unsafe { kevy_set(db, k.as_ptr(), k.len(), b"w".as_ptr(), 1, 30_000) }, 0);
    let pttl = cmd(db, &[b"PTTL", b"fast"]);
    let n: i64 = std::str::from_utf8(&pttl[1..pttl.len() - 2]).unwrap().parse().unwrap();
    assert!(n > 0 && n <= 30_000, "pttl = {n}");
    unsafe { kevy_close(db) };
}

#[test]
fn pubsub_ack_message_and_pattern_frames() {
    let db = kevy_open_mem();
    let sub = unsafe { kevy_subscribe(db, b"c1".as_ptr(), 2) };
    assert!(!sub.is_null());
    let psub = unsafe { kevy_psubscribe(db, b"c*".as_ptr(), 2) };
    assert!(!psub.is_null());

    // Each drains its subscribe ack first.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next(sub, &raw mut out) }, 1);
    assert!(take(out).starts_with(b"*3\r\n$9\r\nsubscribe\r\n"));
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next(psub, &raw mut out) }, 1);
    assert!(take(out).starts_with(b"*3\r\n$10\r\npsubscribe\r\n"));

    // One publish reaches both: message on the channel, pmessage on the glob.
    assert_eq!(cmd(db, &[b"PUBLISH", b"c1", b"hi"]), b":2\r\n");
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next(sub, &raw mut out) }, 1);
    assert_eq!(take(out), b"*3\r\n$7\r\nmessage\r\n$2\r\nc1\r\n$2\r\nhi\r\n");
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next(psub, &raw mut out) }, 1);
    assert_eq!(
        take(out),
        b"*4\r\n$8\r\npmessage\r\n$2\r\nc*\r\n$2\r\nc1\r\n$2\r\nhi\r\n"
    );

    // Drained: 0 with an empty (non-freeable) buffer.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next(sub, &raw mut out) }, 0);
    assert!(out.ptr.is_null());

    unsafe { kevy_sub_close(sub) };
    unsafe { kevy_sub_close(psub) };
    unsafe { kevy_close(db) };
}

#[test]
fn sub_wait_blocks_then_delivers_and_times_out() {
    let db = kevy_open_mem();
    let sub = unsafe { kevy_subscribe(db, b"c1".as_ptr(), 2) };
    assert!(!sub.is_null());

    // The subscribe ack is already queued — wait returns it at once.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_wait(sub, 1000, &raw mut out) }, 1);
    assert!(take(out).starts_with(b"*3\r\n$9\r\nsubscribe\r\n"));

    // Nothing queued now: a bounded wait times out with 0 and an empty buf
    // (and it actually parked — no busy-spin — for ~the timeout).
    let mut out = KevyBuf::empty();
    let t = std::time::Instant::now();
    assert_eq!(unsafe { kevy_sub_wait(sub, 50, &raw mut out) }, 0);
    assert!(out.ptr.is_null());
    assert!(t.elapsed() >= std::time::Duration::from_millis(40));

    // A publish from another thread wakes the waiter.
    let db2 = db as usize;
    let h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        cmd(db2 as *mut _, &[b"PUBLISH", b"c1", b"hi"]);
    });
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_wait(sub, 2000, &raw mut out) }, 1);
    assert_eq!(take(out), b"*3\r\n$7\r\nmessage\r\n$2\r\nc1\r\n$2\r\nhi\r\n");
    h.join().unwrap();

    unsafe { kevy_sub_close(sub) };
    unsafe { kevy_close(db) };
}

#[test]
fn raw_drain_returns_payload_and_framed_lane_is_unchanged() {
    let db = kevy_open_mem();
    // Two independent subscriptions on the same channel: one drained raw,
    // one drained framed — proving both lanes see the same publish and each
    // returns its own shape.
    let raw = unsafe { kevy_subscribe(db, b"c1".as_ptr(), 2) };
    let framed = unsafe { kevy_subscribe(db, b"c1".as_ptr(), 2) };
    assert!(!raw.is_null() && !framed.is_null());

    // The raw lane SKIPS the subscribe ack (a control frame, no payload):
    // with only the ack queued it reports 0 (nothing *with a payload*).
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next_raw(raw, &raw mut out) }, 0);
    assert!(out.ptr.is_null());
    // The framed lane still delivers that ack as a full RESP array.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next(framed, &raw mut out) }, 1);
    assert!(take(out).starts_with(b"*3\r\n$9\r\nsubscribe\r\n"));

    assert_eq!(cmd(db, &[b"PUBLISH", b"c1", b"hello"]), b":2\r\n");

    // Raw lane: exactly the payload bytes, no framing.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next_raw(raw, &raw mut out) }, 1);
    assert_eq!(take(out), b"hello");
    // Framed lane: byte-for-byte the RESP array the server pushes — proof the
    // existing lane is unaffected by the new one.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next(framed, &raw mut out) }, 1);
    assert_eq!(take(out), b"*3\r\n$7\r\nmessage\r\n$2\r\nc1\r\n$5\r\nhello\r\n");

    // Drained: raw reports 0 with an empty (non-freeable) buffer.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next_raw(raw, &raw mut out) }, 0);
    assert!(out.ptr.is_null());

    // Pattern subscriber: raw still hands back just the payload.
    let praw = unsafe { kevy_psubscribe(db, b"c*".as_ptr(), 2) };
    assert_eq!(cmd(db, &[b"PUBLISH", b"c1", b"world"]), b":3\r\n");
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_next_raw(praw, &raw mut out) }, 1);
    assert_eq!(take(out), b"world");

    // Misuse is reported, not undefined.
    let mut misuse = KevyBuf::empty();
    assert!(unsafe { kevy_sub_next_raw(std::ptr::null_mut(), &raw mut misuse) } < 0);
    assert!(unsafe { kevy_sub_next_raw(raw, std::ptr::null_mut()) } < 0);

    unsafe { kevy_sub_close(raw) };
    unsafe { kevy_sub_close(framed) };
    unsafe { kevy_sub_close(praw) };
    unsafe { kevy_close(db) };
}

#[test]
fn sub_wait_raw_skips_ack_blocks_then_delivers_payload() {
    let db = kevy_open_mem();
    let sub = unsafe { kevy_subscribe(db, b"c1".as_ptr(), 2) };
    assert!(!sub.is_null());

    // The subscribe ack is queued but carries no payload: wait_raw reports 0
    // (re-wait), it does NOT surface framing bytes.
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_wait_raw(sub, 1000, &raw mut out) }, 0);
    assert!(out.ptr.is_null());

    // Nothing queued now: a bounded wait times out (0), and it actually parked.
    let mut out = KevyBuf::empty();
    let t = std::time::Instant::now();
    assert_eq!(unsafe { kevy_sub_wait_raw(sub, 50, &raw mut out) }, 0);
    assert!(out.ptr.is_null());
    assert!(t.elapsed() >= std::time::Duration::from_millis(40));

    // A publish from another thread wakes the waiter with just the payload.
    let db2 = db as usize;
    let h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        cmd(db2 as *mut _, &[b"PUBLISH", b"c1", b"payload-only"]);
    });
    let mut out = KevyBuf::empty();
    assert_eq!(unsafe { kevy_sub_wait_raw(sub, 2000, &raw mut out) }, 1);
    assert_eq!(take(out), b"payload-only");
    h.join().unwrap();

    // Misuse.
    let mut misuse = KevyBuf::empty();
    assert!(unsafe { kevy_sub_wait_raw(std::ptr::null_mut(), 0, &raw mut misuse) } < 0);
    assert!(unsafe { kevy_sub_wait_raw(sub, 0, std::ptr::null_mut()) } < 0);

    unsafe { kevy_sub_close(sub) };
    unsafe { kevy_close(db) };
}

#[test]
fn persistent_open_survives_close_and_reopen() {
    let dir = kevy_tmpdir_path();
    let bytes = dir.as_bytes();

    let db = unsafe { kevy_open(bytes.as_ptr(), bytes.len()) };
    assert!(!db.is_null());
    assert_eq!(cmd(db, &[b"SET", b"durable", b"yes"]), b"+OK\r\n");
    unsafe { kevy_close(db) };

    let db = unsafe { kevy_open(bytes.as_ptr(), bytes.len()) };
    assert!(!db.is_null());
    assert_eq!(cmd(db, &[b"GET", b"durable"]), b"$3\r\nyes\r\n");
    unsafe { kevy_close(db) };

    // Invalid UTF-8 and null dirs fail closed.
    assert!(unsafe { kevy_open(b"\xff\xfe".as_ptr(), 2) }.is_null());
    assert!(unsafe { kevy_open(std::ptr::null(), 0) }.is_null());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A per-test unique dir without pulling kevy-tmpdir into the deps of a
/// crate whose whole point is a minimal surface.
fn kevy_tmpdir_path() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("kevy-ffi-abi-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p.join("data").to_str().unwrap().to_owned()
}
