//! The polled pub/sub surface — the framed lane.
//!
//! `lib.rs`'s own doc explains why pub/sub here is polled rather than
//! called back: a callback crossing the FFI on the publisher's thread is a
//! reentrancy and GC-interop hazard in Go and C#. These are the five entry
//! points that model asks for — open, open-by-pattern, drain, wait, close —
//! each frame handed over RESP-encoded, the same array the server pushes.
//!
//! Split out of `lib.rs` for the house 500-LOC rule, the same way
//! `sub_raw.rs` was; that file is this one's scalar sibling, handing back
//! the payload with no RESP framing.

// `catch_unwind` at the ABI boundary. Its `Err` is the panic payload,
// and the point of catching it here is that a panic must not cross
// into C — see `boundary/no-panic-across-abi`. There is no Rust
// frame above this to hand it to, and the callee has already
// reported through its own error channel.
#![expect(
    clippy::let_underscore_must_use,
    reason = "catch_unwind exists to stop the unwind, not to report it"
)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use kevy_embedded::KevyError;

use crate::frame::encode_frame;
use crate::{KevyBuf, KevyDb, KevySub};

/// Open a subscription on one channel (call again for more channels — or
/// subscribe to a pattern with [`kevy_psubscribe`]). Returns null on error.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_open_mem, kevy_publish};
/// use kevy_ffi::{kevy_sub_close, kevy_sub_next, kevy_subscribe};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; every name pointer covers its length; each frame
/// // is read before its single free; each handle is closed once.
/// unsafe {
///     let sub = kevy_subscribe(db, b"news".as_ptr(), 4);
///     assert_eq!(kevy_sub_next(sub, &mut out), 1); // the subscribe ack
///     assert_eq!(std::slice::from_raw_parts(out.ptr, out.len), b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n");
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     kevy_sub_close(sub);
///     kevy_close(db);
/// }
/// ```
///
/// # Safety
/// `chan` must point to `chan_len` readable bytes; `db` must be live.
// NO-UNWIND: the subscription is opened inside sub_open, which catches
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_subscribe(
    db: *mut KevyDb,
    chan: *const u8,
    chan_len: usize,
) -> *mut KevySub {
    // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
    // the checks above.
    unsafe { sub_open(db, chan, chan_len, false) }
}

/// Open a subscription on one glob pattern (`room:*`). Returns null on error.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_open_mem, kevy_publish};
/// use kevy_ffi::{kevy_psubscribe, kevy_sub_close, kevy_sub_next};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; every name pointer covers its length; each frame
/// // is read before its single free; each handle is closed once.
/// unsafe {
///     let sub = kevy_psubscribe(db, b"room:*".as_ptr(), 6);
///     assert_eq!(kevy_publish(db, b"room:1".as_ptr(), 6, b"hi".as_ptr(), 2), 1);
///     assert_eq!(kevy_sub_next(sub, &mut out), 1); // the psubscribe ack
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     assert_eq!(kevy_sub_next(sub, &mut out), 1);
///     let frame = std::slice::from_raw_parts(out.ptr, out.len);
///     assert_eq!(frame, b"*4\r\n$8\r\npmessage\r\n$6\r\nroom:*\r\n$6\r\nroom:1\r\n$2\r\nhi\r\n");
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     kevy_sub_close(sub);
///     kevy_close(db);
/// }
/// ```
///
/// # Safety
/// Same contract as [`kevy_subscribe`].
// NO-UNWIND: the subscription is opened inside sub_open, which catches
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_psubscribe(
    db: *mut KevyDb,
    pat: *const u8,
    pat_len: usize,
) -> *mut KevySub {
    // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
    // the checks above.
    unsafe { sub_open(db, pat, pat_len, true) }
}

unsafe fn sub_open(
    db: *mut KevyDb,
    chan: *const u8,
    chan_len: usize,
    pattern: bool,
) -> *mut KevySub {
    if db.is_null() || chan.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    // SAFETY: the `# Safety` contract above covers this pointer/length pair.
    let name = unsafe { std::slice::from_raw_parts(chan, chan_len) };
    let opened = catch_unwind(AssertUnwindSafe(|| {
        if pattern { store.psubscribe(&[name]) } else { store.subscribe(&[name]) }
    }));
    match opened {
        Ok(sub) => Box::into_raw(Box::new(KevySub { sub })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Drain one pending pub/sub frame without blocking.
///
/// Returns 1 with `out` holding the frame encoded as the RESP array the
/// server would push (`*3 … message <channel> <payload>`, `*4 … pmessage
/// <pattern> <channel> <payload>`, and the subscribe/unsubscribe acks);
/// 0 when nothing is queued; negative on misuse.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_open_mem, kevy_publish};
/// use kevy_ffi::{kevy_sub_close, kevy_sub_next, kevy_subscribe};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; every name pointer covers its length; each frame
/// // is read before its single free; each handle is closed once.
/// unsafe {
///     let sub = kevy_subscribe(db, b"news".as_ptr(), 4);
///     kevy_publish(db, b"news".as_ptr(), 4, b"hi".as_ptr(), 2);
///     let mut frames = Vec::new();
///     while kevy_sub_next(sub, &mut out) == 1 {
///         frames.push(std::slice::from_raw_parts(out.ptr, out.len).to_vec());
///         kevy_buf_free(out.ptr, out.len, out.cap);
///     }
///     assert_eq!(frames.len(), 2); // the subscribe ack, then the message
///     assert_eq!(frames[1], b"*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n");
///     kevy_sub_close(sub);
///     kevy_close(db);
/// }
/// ```
///
/// # Safety
/// `sub` must be live; `out` must point to writable [`KevyBuf`] storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_next(sub: *mut KevySub, out: *mut KevyBuf) -> i32 {
    if out.is_null() {
        return -1;
    }
    // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
    // the checks above.
    unsafe { out.write(KevyBuf::empty()) };
    if sub.is_null() {
        return -1;
    }
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let s = unsafe { &(*sub).sub };
    let polled = catch_unwind(AssertUnwindSafe(|| s.try_recv()));
    match polled {
        Ok(Ok(Some(frame))) => {
            // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
            // the checks above.
            unsafe { out.write(KevyBuf::from_vec(encode_frame(&frame))) };
            1
        }
        Ok(Ok(None)) => 0,
        _ => -2,
    }
}

/// Block until one pub/sub frame is queued, or `timeout_ms` elapses.
///
/// Same output/return shape as [`kevy_sub_next`], but instead of returning
/// 0 the instant the queue is empty it **parks the calling thread** (no
/// busy-poll) up to `timeout_ms`: 1 with `out` holding the frame, 0 on
/// timeout, negative on misuse or once the bus tears down. `timeout_ms == 0`
/// waits indefinitely (until a frame arrives or the bus is gone).
///
/// The engine already parks efficiently on its `mpsc` channel; this just
/// exposes that wait to the C ABI so a push-style binding (a poller thread
/// that hops each frame onto a host runtime) can block in the kernel
/// instead of spinning `kevy_sub_next` and burning a core.
///
/// ```
/// use kevy_ffi::{KevyBuf, kevy_buf_free, kevy_close, kevy_open_mem, kevy_publish};
/// use kevy_ffi::{kevy_sub_close, kevy_sub_next, kevy_sub_wait, kevy_subscribe};
///
/// let db = kevy_open_mem();
/// let mut out = KevyBuf { ptr: std::ptr::null_mut(), len: 0, cap: 0 };
/// // SAFETY: `db` is live; every name pointer covers its length; each frame
/// // is read before its single free; each handle is closed once.
/// unsafe {
///     let sub = kevy_subscribe(db, b"news".as_ptr(), 4);
///     assert_eq!(kevy_sub_next(sub, &mut out), 1); // drain the subscribe ack
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     assert_eq!(kevy_sub_wait(sub, 10, &mut out), 0); // nothing within 10 ms
///     kevy_publish(db, b"news".as_ptr(), 4, b"hi".as_ptr(), 2);
///     assert_eq!(kevy_sub_wait(sub, 1_000, &mut out), 1);
///     assert!(std::slice::from_raw_parts(out.ptr, out.len).ends_with(b"$2\r\nhi\r\n"));
///     kevy_buf_free(out.ptr, out.len, out.cap);
///     kevy_sub_close(sub);
///     kevy_close(db);
/// }
/// ```
///
/// # Safety
/// `sub` must be live; `out` must point to writable [`KevyBuf`] storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_wait(
    sub: *mut KevySub,
    timeout_ms: u64,
    out: *mut KevyBuf,
) -> i32 {
    if out.is_null() {
        return -1;
    }
    // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
    // the checks above.
    unsafe { out.write(KevyBuf::empty()) };
    if sub.is_null() {
        return -1;
    }
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let s = unsafe { &(*sub).sub };
    let waited = catch_unwind(AssertUnwindSafe(|| {
        if timeout_ms == 0 {
            s.recv().map(Some)
        } else {
            match s.recv_timeout(std::time::Duration::from_millis(timeout_ms)) {
                Ok(f) => Ok(Some(f)),
                Err(KevyError::TimedOut) => Ok(None),
                Err(e) => Err(e),
            }
        }
    }));
    match waited {
        Ok(Ok(Some(frame))) => {
            // SAFETY: covered by this fn's `# Safety` contract, with the null case ruled out by
            // the checks above.
            unsafe { out.write(KevyBuf::from_vec(encode_frame(&frame))) };
            1
        }
        Ok(Ok(None)) => 0, // timeout, nothing arrived
        _ => -2,           // bus closed or panic
    }
}

/// Close a subscription (unsubscribes from everything it held). Null is a
/// no-op.
///
/// ```
/// use kevy_ffi::{kevy_close, kevy_open_mem, kevy_publish, kevy_sub_close, kevy_subscribe};
///
/// let db = kevy_open_mem();
/// // SAFETY: `db` is live; channel/payload pointers cover their lengths;
/// // the subscription is closed once, then `db`; null is a no-op.
/// unsafe {
///     let sub = kevy_subscribe(db, b"news".as_ptr(), 4);
///     assert_eq!(kevy_publish(db, b"news".as_ptr(), 4, b"x".as_ptr(), 1), 1);
///     kevy_sub_close(sub);
///     assert_eq!(kevy_publish(db, b"news".as_ptr(), 4, b"x".as_ptr(), 1), 0); // unsubscribed
///     kevy_sub_close(std::ptr::null_mut());
///     kevy_close(db);
/// }
/// ```
///
/// # Safety
/// `sub` must be a live handle from this library, passed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_close(sub: *mut KevySub) {
    if sub.is_null() {
        return;
    }
    // SAFETY: the contract makes the caller pass a handle this crate produced with
    // `Box::into_raw` and never freed, so this takes ownership back exactly once.
    let _ = catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(sub) })));
}
