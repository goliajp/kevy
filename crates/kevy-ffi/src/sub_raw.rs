//! Scalar (RESP-free) pub/sub drain — the pub/sub analog of `kevy_get`.
//!
//! `kevy_sub_next` returns each frame RESP-encoded (`*3…message…`), which a
//! known-channel push subscriber does not want: it only needs the payload
//! bytes. These two entry points hand back **just the payload**, moved out of
//! the delivered [`PubsubFrame`] with no RESP framing and no re-copy — the
//! `encode_frame` cost the framed lane pays per frame is gone. Split out of
//! `lib.rs` for the house 500-LOC rule; a NEW lane, additive to the framed one.

use std::panic::{AssertUnwindSafe, catch_unwind};

use kevy_embedded::KevyError;

use crate::{KevyBuf, KevySub};

/// Scalar pub/sub drain: the raw message **payload**, no RESP framing — the
/// pub/sub analog of [`kevy_get`](crate::kevy_get). Drains non-blocking,
/// skipping control/ack frames (subscribe/unsubscribe, which carry no payload)
/// until it reaches a delivery frame, whose payload it writes to `out`
/// (returning 1). Returns 0 once the queue empties with no delivery frame, and
/// negative on misuse or a torn-down bus.
///
/// The payload `Vec` is **moved** out of the frame into `out` — one fewer alloc
/// and no re-copy versus [`kevy_sub_next`](crate::kevy_sub_next)'s
/// `encode_frame`. The caller loses the channel and the message-vs-pmessage
/// distinction, and never sees subscribe/unsubscribe acks: this is the lane for
/// a known-channel push subscriber that only wants the bytes. A pattern
/// subscriber that needs the channel keeps using the framed
/// [`kevy_sub_next`](crate::kevy_sub_next).
///
/// # Safety
/// `sub` must be live; `out` must point to writable [`KevyBuf`] storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_next_raw(sub: *mut KevySub, out: *mut KevyBuf) -> i32 {
    if out.is_null() {
        return -1;
    }
    unsafe { out.write(KevyBuf::empty()) };
    if sub.is_null() {
        return -1;
    }
    let s = unsafe { &(*sub).sub };
    let drained = catch_unwind(AssertUnwindSafe(|| {
        loop {
            match s.try_recv() {
                Ok(Some(f)) => {
                    if let Some(p) = f.into_payload() {
                        return Ok(Some(p));
                    }
                    // control/ack frame — no payload; keep draining.
                }
                Ok(None) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }));
    match drained {
        Ok(Ok(Some(p))) => {
            unsafe { out.write(KevyBuf::from_vec(p)) };
            1
        }
        Ok(Ok(None)) => 0,
        _ => -2,
    }
}

/// Blocking scalar drain: [`kevy_sub_next_raw`] that parks up to `timeout_ms`
/// (0 = indefinitely) instead of returning 0 on an empty queue. Returns 1 with
/// the raw payload in `out` when a delivery frame arrives; 0 when the wait
/// times out **or** the waking frame was a control/ack (no payload) — the
/// caller simply waits again (the poller idiom `if rc != 1 { continue }`).
/// Negative once the bus tears down or on misuse.
///
/// Same payload-move + lost-channel semantics as [`kevy_sub_next_raw`]. The
/// blocking companion so a push poller can run a fully RESP-free lane: park in
/// `kevy_sub_wait_raw` for the first frame, then drain the rest with
/// `kevy_sub_next_raw`.
///
/// # Safety
/// `sub` must be live; `out` must point to writable [`KevyBuf`] storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_sub_wait_raw(
    sub: *mut KevySub,
    timeout_ms: u64,
    out: *mut KevyBuf,
) -> i32 {
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
        // A delivery frame carries a payload; a control frame does not — the
        // latter is reported as 0 so the caller re-waits (payload-only lane).
        Ok(Ok(Some(f))) => match f.into_payload() {
            Some(p) => {
                unsafe { out.write(KevyBuf::from_vec(p)) };
                1
            }
            None => 0,
        },
        Ok(Ok(None)) => 0, // timeout, nothing arrived
        _ => -2,           // bus closed or panic
    }
}
