//! JNI gate for kevy — Android and desktop JVMs share this one cdylib.
//!
//! A thin shell over `kevy-ffi` (linked as a plain Rust rlib, not over the
//! C ABI): every export below is `Java_jp_golia_kevy_KevyNative_<name>`,
//! matching the `static native` methods of `jp.golia.kevy.KevyNative`
//! (bindings/android/java). Two decisions keep it thin:
//!
//! - **Four JNIEnv slots total.** The Java side packs argv into one flat
//!   `byte[]` (u32-LE length prefix per argument), so the JNI surface is
//!   `byte[]` in / `byte[]` out and only `GetArrayLength` / `NewByteArray`
//!   / `GetByteArrayRegion` / `SetByteArrayRegion` are ever called — see
//!   [`env`] for the hand-counted slot table.
//! - **Handles are `jlong`.** `*mut KevyDb` / `*mut KevySub` travel as
//!   opaque longs; 0 means failure, exactly like null on the C ABI.
//!
//! Every entry point catches panics (unwinding into the JVM is UB) and
//! reports failure through its normal channel: 0 for handles, null for
//! `byte[]`, negative for status ints. The `JNIEnv` pointer is only used
//! within the call that received it, as the JNI spec requires.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

use kevy_ffi::{KevyBuf, KevyDb, KevySub, unpack_argv};

mod env;
use env::{JBoolean, JInt, JLong, JObject, JniEnv, get_byte_array, new_byte_array};

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
        unsafe { new_byte_array(env, &[]) }
    } else {
        let s = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
        unsafe { new_byte_array(env, s) }
    };
    unsafe { kevy_ffi::kevy_buf_free(buf.ptr, buf.len, buf.cap) };
    arr
}

/// `KevyNative.open(byte[] dir)` — open a persistent store rooted at the
/// UTF-8 path in `dir`. Returns the db handle, 0 on failure.
///
/// # Safety
/// Called by the JVM only: `env` must be the current thread's `JNIEnv *`
/// and `dir` a live `byte[]` reference (or null, which fails cleanly).
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_open")]
pub unsafe extern "system" fn jni_open(env: JniEnv, _class: JObject, dir: JObject) -> JLong {
    catch_unwind(AssertUnwindSafe(|| {
        if dir.is_null() {
            return 0;
        }
        let d = unsafe { get_byte_array(env, dir) };
        handle(unsafe { kevy_ffi::kevy_open(d.as_ptr(), d.len()) })
    }))
    .unwrap_or(0)
}

/// `KevyNative.openMem()` — open a pure in-memory store. 0 on failure.
///
/// # Safety
/// Called by the JVM only.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_openMem")]
pub unsafe extern "system" fn jni_open_mem(_env: JniEnv, _class: JObject) -> JLong {
    catch_unwind(|| handle(kevy_ffi::kevy_open_mem())).unwrap_or(0)
}

/// `KevyNative.close(long db)` — close a store handle. 0 is a no-op; the
/// handle must not be used afterwards.
///
/// # Safety
/// Called by the JVM only; `db` must be a live handle from this library,
/// passed exactly once.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_close")]
pub unsafe extern "system" fn jni_close(_env: JniEnv, _class: JObject, db: JLong) {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { kevy_ffi::kevy_close(db_ptr(db)) }));
}

/// `KevyNative.cmd(long db, byte[] packedArgv)` — run one command; argv is
/// packed per [`unpack_argv`]. Returns the RESP-encoded reply (a protocol
/// error is still a reply), or null on misuse.
///
/// # Safety
/// Called by the JVM only: `env` / `packedArgv` live for this call, `db` a
/// live handle.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_cmd")]
pub unsafe extern "system" fn jni_cmd(
    env: JniEnv,
    _class: JObject,
    db: JLong,
    packed: JObject,
) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 || packed.is_null() {
            return null_mut();
        }
        let bytes = unsafe { get_byte_array(env, packed) };
        let Some(args) = unpack_argv(&bytes) else {
            return null_mut();
        };
        let ptrs: Vec<*const u8> = args.iter().map(|a| a.as_ptr()).collect();
        let lens: Vec<usize> = args.iter().map(Vec::len).collect();
        let mut out = empty_buf();
        let rc = unsafe {
            kevy_ffi::kevy_cmd(db_ptr(db), args.len(), ptrs.as_ptr(), lens.as_ptr(), &mut out)
        };
        if rc != 0 {
            return null_mut();
        }
        unsafe { take_buf(env, out) }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.get(long db, byte[] key)` — scalar fast-path GET: the raw
/// value bytes, or null on a miss (and on misuse).
///
/// # Safety
/// Called by the JVM only, same contract as [`jni_cmd`].
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_get")]
pub unsafe extern "system" fn jni_get(
    env: JniEnv,
    _class: JObject,
    db: JLong,
    key: JObject,
) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 || key.is_null() {
            return null_mut();
        }
        let k = unsafe { get_byte_array(env, key) };
        let mut out = empty_buf();
        let rc = unsafe { kevy_ffi::kevy_get(db_ptr(db), k.as_ptr(), k.len(), &mut out) };
        if rc == 1 { unsafe { take_buf(env, out) } } else { null_mut() }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.set(long db, byte[] key, byte[] val, long ttlMs)` — scalar
/// fast-path SET (`ttlMs` 0 or negative = no expiry). 0 on success,
/// negative on misuse.
///
/// # Safety
/// Called by the JVM only, same contract as [`jni_cmd`].
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_set")]
pub unsafe extern "system" fn jni_set(
    env: JniEnv,
    _class: JObject,
    db: JLong,
    key: JObject,
    val: JObject,
    ttl_ms: JLong,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 || key.is_null() || val.is_null() {
            return -1;
        }
        let k = unsafe { get_byte_array(env, key) };
        let v = unsafe { get_byte_array(env, val) };
        let ttl = if ttl_ms > 0 { ttl_ms as u64 } else { 0 };
        unsafe { kevy_ffi::kevy_set(db_ptr(db), k.as_ptr(), k.len(), v.as_ptr(), v.len(), ttl) }
    }))
    .unwrap_or(-2)
}

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
        let c = unsafe { get_byte_array(env, chan) };
        let sub = if pattern != 0 {
            unsafe { kevy_ffi::kevy_psubscribe(db_ptr(db), c.as_ptr(), c.len()) }
        } else {
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
        let rc = unsafe { kevy_ffi::kevy_sub_next(sub_ptr(sub), &mut out) };
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
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { kevy_ffi::kevy_sub_close(sub_ptr(sub)) }));
}
