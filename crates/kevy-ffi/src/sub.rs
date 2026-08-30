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

use std::panic::{AssertUnwindSafe, catch_unwind};

use kevy_embedded::KevyError;

use crate::frame::encode_frame;
use crate::{KevyBuf, KevyDb, KevySub};

/// Open a subscription on one channel (call again for more channels — or
/// subscribe to a pattern with [`kevy_psubscribe`]). Returns null on error.
///
/// # Safety
/// `chan` must point to `chan_len` readable bytes; `db` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_subscribe(
    db: *mut KevyDb,
    chan: *const u8,
    chan_len: usize,
) -> *mut KevySub {
    unsafe { sub_open(db, chan, chan_len, false) }
}

/// Open a subscription on one glob pattern (`room:*`). Returns null on error.
///
/// # Safety
/// Same contract as [`kevy_subscribe`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_psubscribe(
    db: *mut KevyDb,
    pat: *const u8,
    pat_len: usize,
) -> *mut KevySub {
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
    let store = unsafe { &(*db).store };
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
/// # Safety
/// `sub` must be live; `out` must point to writable [`KevyBuf`] storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_next(sub: *mut KevySub, out: *mut KevyBuf) -> i32 {
    if out.is_null() {
        return -1;
    }
    unsafe { out.write(KevyBuf::empty()) };
    if sub.is_null() {
        return -1;
    }
    let s = unsafe { &(*sub).sub };
    let polled = catch_unwind(AssertUnwindSafe(|| s.try_recv()));
    match polled {
        Ok(Ok(Some(frame))) => {
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
/// # Safety
/// `sub` must be live; `out` must point to writable [`KevyBuf`] storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_wait(sub: *mut KevySub, timeout_ms: u64, out: *mut KevyBuf) -> i32 {
    if out.is_null() {
        return -1;
    }
    unsafe { out.write(KevyBuf::empty()) };
    if sub.is_null() {
        return -1;
    }
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
/// # Safety
/// `sub` must be a live handle from this library, passed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_close(sub: *mut KevySub) {
    if sub.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(sub) })));
}
