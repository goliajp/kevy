//! The subscription family — open, poll, wait, close.

use crate::napi::*;
use crate::{empty_buf, take_buf};
use kevy_ffi::{KevyDb, KevySub};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

/// `subscribe(db, chanBuffer)` — polled subscription on one channel.
pub(crate) unsafe extern "C" fn js_subscribe(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `env` is this callback's and the arguments are live locals.
    unsafe { sub_open(env, info, false) }
}

/// `psubscribe(db, patternBuffer)` — polled subscription on one glob pattern.
pub(crate) unsafe extern "C" fn js_psubscribe(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    // SAFETY: `env` is this callback's and the arguments are live locals.
    unsafe { sub_open(env, info, true) }
}

unsafe fn sub_open(env: NapiEnv, info: NapiCallbackInfo, pattern: bool) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext, chan] = unsafe { args::<2>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        // SAFETY: `env` is this callback's; the value came from `args` on the same call.
        let Some(c) = (unsafe { buffer_bytes(env, chan) }) else {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: subscribe needs a Buffer channel\0") };
        };
        let sub = if pattern {
            // SAFETY: the handle is a live external and each pointer/length pair is a local.
            unsafe { kevy_ffi::kevy_psubscribe(db, c.as_ptr(), c.len()) }
        } else {
            // SAFETY: the handle is a live external and each pointer/length pair is a local.
            unsafe { kevy_ffi::kevy_subscribe(db, c.as_ptr(), c.len()) }
        };
        if sub.is_null() {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: subscribe failed\0") };
        }
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        unsafe { make_external(env, sub) }
    }))
    .unwrap_or(null_mut())
}

/// `subNext(sub)` — drain one pending pub/sub frame (a RESP-array Buffer),
/// or `undefined` when the queue is empty.
pub(crate) unsafe extern "C" fn js_sub_next(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext] = unsafe { args::<1>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let sub: *mut KevySub = unsafe { external_ptr(env, ext) };
        let mut out = empty_buf();
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        let rc = unsafe { kevy_ffi::kevy_sub_next(sub, &mut out) };
        match rc {
            // SAFETY: `env` is this callback's; `out` is the buffer kevy-ffi just filled and
            // this consumes it exactly once.
            1 => unsafe { take_buf(env, out) },
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            0 => unsafe { undefined(env) },
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            _ => unsafe { throw(env, "kevy: subscription misuse\0") },
        }
    }))
    .unwrap_or(null_mut())
}

/// `subWait(sub, timeoutMs)` — block up to `timeoutMs` (0 = forever) for one
/// pub/sub frame, parking in the kernel instead of spinning. Returns the
/// RESP-array frame Buffer (`rc == 1`), `undefined` on timeout (`rc == 0`), and
/// throws on bus-gone / misuse (`rc < 0`). The blocking twin of [`js_sub_next`].
/// The frame bytes are Vec-encoded, so this frees through the PLAIN [`take_buf`],
/// not [`take_buf_shared`].
///
/// **BLOCKS the calling thread** — it parks the OS thread until a frame arrives
/// or the timeout elapses. Only safe on a dedicated `worker_thread`; calling it
/// on the main event loop stalls all of Node (timers, I/O, everything) for the
/// whole wait.
///
/// # Safety
/// Called by Node only; `env` / `info` live for this call.
pub(crate) unsafe extern "C" fn js_sub_wait(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext, timeout] = unsafe { args::<2>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let sub: *mut KevySub = unsafe { external_ptr(env, ext) };
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        let timeout_ms = unsafe { get_i64(env, timeout) };
        let timeout_ms = if timeout_ms > 0 { timeout_ms as u64 } else { 0 };
        let mut out = empty_buf();
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        let rc = unsafe { kevy_ffi::kevy_sub_wait(sub, timeout_ms, &mut out) };
        match rc {
            // SAFETY: `env` is this callback's; `out` is the buffer kevy-ffi just filled and
            // this consumes it exactly once.
            1 => unsafe { take_buf(env, out) },
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            0 => unsafe { undefined(env) },
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            _ => unsafe { throw(env, "kevy: subscription misuse\0") },
        }
    }))
    .unwrap_or(null_mut())
}

/// `subClose(sub)` — close a subscription handle.
pub(crate) unsafe extern "C" fn js_sub_close(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext] = unsafe { args::<1>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let sub: *mut KevySub = unsafe { external_ptr(env, ext) };
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        unsafe { kevy_ffi::kevy_sub_close(sub) };
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        unsafe { undefined(env) }
    }))
    .unwrap_or(null_mut())
}
