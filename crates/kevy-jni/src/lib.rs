//! JNI gate for kevy — Android and desktop JVMs share this one cdylib.
//!
//! A thin shell over `kevy-ffi` (linked as a plain Rust rlib, not over the
//! C ABI): every export below is `Java_jp_golia_kevy_KevyNative_<name>`,
//! matching the `static native` methods of `jp.golia.kevy.KevyNative`
//! (bindings/android/java). Two decisions keep it thin:
//!
//! - **A handful of JNIEnv slots.** The Java side packs argv into one flat
//!   `byte[]` (u32-LE length prefix per argument), so the JNI surface is
//!   `byte[]` in / `byte[]` out (plus a `long[]` out for the open report
//!   and the scalar GET's signal exception) — see [`env`] for the
//!   hand-counted slot table.
//! - **Handles are `jlong`.** `*mut KevyDb` / `*mut KevySub` travel as
//!   opaque longs; 0 means failure, exactly like null on the C ABI.
//!
//! Every entry point catches panics (unwinding into the JVM is UB) and
//! reports failure through its normal channel: 0 for handles, null for
//! `byte[]`, negative for status ints. The `JNIEnv` pointer is only used
//! within the call that received it, as the JNI spec requires.

//!
//! # Safety of the `unsafe` blocks below
//!
//! Every `unsafe` block here rests on three facts, stated once instead of at
//! each call site:
//!
//! 1. **`env` is this call's JNI env.** The JVM invokes each `Java_*` export
//!    with a live `JNIEnv*` — a pointer to the function table — valid on the
//!    calling thread for the duration of that call and no longer.
//! 2. **Slots are the table JNI specifies.** `slot(env, idx)` reads the
//!    function pointer at a hand-counted index, and the `*Fn` type alias it is
//!    transmuted to is that slot's documented signature.
//! 3. **References and buffers are live.** Array references come from the JVM
//!    with the call; every buffer written into is a local sized from the length
//!    JNI just reported for that array.
//!
//! Panics are caught at every export because unwinding into the JVM is
//! undefined behaviour; that is a property of this file, not of any one block.
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

use kevy_ffi::{KevyBuf, KevyDb, KevySub};

mod env;
mod kv;
mod lifecycle;
mod sub;
use env::{JLong, JObject, JniEnv, new_byte_array};

/// Rebuild a store pointer from the opaque handle Java carries.
fn db_ptr(handle: JLong) -> *mut KevyDb {
    handle as usize as *mut KevyDb
}

/// Rebuild a subscription pointer from its opaque handle.
fn sub_ptr(handle: JLong) -> *mut KevySub {
    handle as usize as *mut KevySub
}

/// Collapse a pointer into the opaque handle Java carries (0 = null).
fn handle<T>(p: *mut T) -> JLong {
    p as usize as JLong
}

const fn empty_buf() -> KevyBuf {
    KevyBuf { ptr: null_mut(), len: 0, cap: 0 }
}

/// Copy a reply buffer into a fresh `byte[]`, then free the buffer.
///
/// # Safety
/// `env` must be the current call's `JNIEnv *`; `buf` must be exactly as
/// returned by a kevy-ffi call, consumed exactly once.
unsafe fn take_buf(env: JniEnv, buf: KevyBuf) -> JObject {
    let arr = if buf.len == 0 {
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        unsafe { new_byte_array(env, &[]) }
    } else {
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let s = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        unsafe { new_byte_array(env, s) }
    };
    // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
    // local still in scope, which is what the callee's `# Safety` requires.
    unsafe { kevy_ffi::kevy_buf_free(buf.ptr, buf.len, buf.cap) };
    arr
}

/// Copy a **shared-lane** reply buffer into a fresh `byte[]`, then free it
/// through the shared reclaimer. Mirrors [`take_buf`] but pairs with
/// [`kevy_ffi::kevy_buf_free_shared`], never [`kevy_ffi::kevy_buf_free`]:
/// the shared GET hands back a view whose `cap` is an OPAQUE owner handle (an
/// `Arc` raw pointer for bulk, a tagged `Vec` capacity for small), so routing
/// it through the plain free would corrupt the allocator — UB. The bytes are
/// still copied into the JVM-owned array (a `byte[]` must own its storage);
/// what the shared lane saves is the engine-side clone into a fresh `Vec`.
///
/// # Safety
/// `env` must be the current call's `JNIEnv *`; `buf` must be exactly as
/// returned by [`kevy_ffi::kevy_get_shared`], consumed exactly once.
unsafe fn take_buf_shared(env: JniEnv, buf: KevyBuf) -> JObject {
    let arr = if buf.len == 0 {
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        unsafe { new_byte_array(env, &[]) }
    } else {
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let s = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        unsafe { new_byte_array(env, s) }
    };
    // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
    // local still in scope, which is what the callee's `# Safety` requires.
    unsafe { kevy_ffi::kevy_buf_free_shared(buf.ptr, buf.len, buf.cap) };
    arr
}

/// `KevyNative.version()` — the engine version as UTF-8 bytes.
///
/// # Safety
/// Called by the JVM only.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_version")]
pub unsafe extern "system" fn jni_version(env: JniEnv, _class: JObject) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
        // local still in scope, which is what the callee's `# Safety` requires.
        let v = unsafe { std::ffi::CStr::from_ptr(kevy_ffi::kevy_version()) };
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        unsafe { new_byte_array(env, v.to_bytes()) }
    }))
    .unwrap_or(null_mut())
}
