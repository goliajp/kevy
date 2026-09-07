//! The subscription family — open, poll, wait, close.

use crate::env::*;
use crate::{db_ptr, empty_buf, handle, sub_ptr, take_buf};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

/// `KevyNative.subscribe(long db, byte[] chan, boolean pattern)` — open a
/// polled subscription on one channel (or glob pattern). Returns the
/// subscription handle, 0 on failure.
///
/// # Safety
/// Called by the JVM only, same contract as [`jni_cmd`].
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_subscribe")]
pub unsafe extern "system" fn jni_subscribe(
    env: JniEnv,
    _class: JObject,
    db: JLong,
    chan: JObject,
    pattern: JBoolean,
) -> JLong {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 || chan.is_null() {
            return 0;
        }
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let c = unsafe { get_byte_array(env, chan) };
        let sub = if pattern != 0 {
            // SAFETY: `env` is this call's, `arr` is the live array reference JNI passed, and
            // the buffer is a local sized to the length just read from that array.
            unsafe { kevy_ffi::kevy_psubscribe(db_ptr(db), c.as_ptr(), c.len()) }
        } else {
            // SAFETY: `env` is this call's, `arr` is the live array reference JNI passed, and
            // the buffer is a local sized to the length just read from that array.
            unsafe { kevy_ffi::kevy_subscribe(db_ptr(db), c.as_ptr(), c.len()) }
        };
        handle(sub)
    }))
    .unwrap_or(0)
}

/// `KevyNative.subNext(long sub)` — drain one pending pub/sub frame,
/// encoded as the RESP array the server would push. Null when the queue is
/// empty (and on misuse).
///
/// # Safety
/// Called by the JVM only; `sub` must be a live subscription handle.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_subNext")]
pub unsafe extern "system" fn jni_sub_next(env: JniEnv, _class: JObject, sub: JLong) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        if sub == 0 {
            return null_mut();
        }
        let mut out = empty_buf();
        // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
        // local still in scope, which is what the callee's `# Safety` requires.
        let rc = unsafe { kevy_ffi::kevy_sub_next(sub_ptr(sub), &mut out) };
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        if rc == 1 { unsafe { take_buf(env, out) } } else { null_mut() }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.subWait(long sub, long timeoutMs)` — block up to `timeoutMs`
/// (0 = forever) for one frame, parking in the kernel instead of spinning.
/// Returns the RESP-array frame bytes, or null on timeout / bus-gone / misuse.
/// The blocking twin of [`jni_sub_next`]; lets a JVM subscriber wait without
/// a busy poll loop.
///
/// # Safety
/// Called by the JVM only; `sub` must be a live handle (or 0).
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_subWait")]
pub unsafe extern "system" fn jni_sub_wait(
    env: JniEnv,
    _class: JObject,
    sub: JLong,
    timeout_ms: JLong,
) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        if sub == 0 {
            return null_mut();
        }
        let mut out = empty_buf();
        // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
        // local still in scope, which is what the callee's `# Safety` requires.
        let rc = unsafe { kevy_ffi::kevy_sub_wait(sub_ptr(sub), timeout_ms as u64, &mut out) };
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        if rc == 1 { unsafe { take_buf(env, out) } } else { null_mut() }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.subClose(long sub)` — close a subscription handle. 0 is a
/// no-op; the handle must not be used afterwards.
///
/// # Safety
/// Called by the JVM only; `sub` must be a live handle, passed exactly once.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_subClose")]
pub unsafe extern "system" fn jni_sub_close(_env: JniEnv, _class: JObject, sub: JLong) {
    // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
    // local still in scope, which is what the callee's `# Safety` requires.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { kevy_ffi::kevy_sub_close(sub_ptr(sub)) }));
}
