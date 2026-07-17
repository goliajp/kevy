//! Hand-written `JNIEnv` access — no `jni` crate, the same discipline as
//! `kevy-sys` binding libc by hand.
//!
//! A native method receives `JNIEnv *env`, and `JNIEnv` is a pointer to
//! the VM's interface struct (`typedef const struct JNINativeInterface_
//! *JNIEnv;`, Zulu 17 jni.h:197). That struct is one flat table of
//! function pointers opening with four reserved slots (jni.h:214-219), so
//! indexing the table by slot number is the entire binding. The gate keeps
//! its JNI surface down to `byte[]` in / `byte[]` out plus one pair to raise
//! a signal exception (the scalar GET's wrong-type fallback) and one pair to
//! build the open report's `long[]`, for eight slots. Each index below was
//! counted out of the real headers — `reserved0` = slot 0, `GetVersion` =
//! slot 4 — and the count is identical in Zulu JDK 17 `include/jni.h` and
//! Android NDK r27 `sysroot/usr/include/jni.h`:
//!
//! | slot | function             | declared at (Zulu 17 jni.h) |
//! |-----:|----------------------|-----------------------------|
//! |    6 | `FindClass`          | line 225                    |
//! |   14 | `ThrowNew`           | line 246                    |
//! |  171 | `GetArrayLength`     | line 625                    |
//! |  176 | `NewByteArray`       | line 637                    |
//! |  180 | `NewLongArray`       | line 645                    |
//! |  200 | `GetByteArrayRegion` | line 688                    |
//! |  208 | `SetByteArrayRegion` | line 705                    |
//! |  212 | `SetLongArrayRegion` | line 713                    |

use std::ffi::{CStr, c_char, c_void};

/// An opaque JVM local reference (`jobject` / `jbyteArray` / `jclass`).
pub(crate) type JObject = *mut c_void;
/// `jint` (jni_md.h:57 `typedef int jint`).
pub(crate) type JInt = i32;
/// `jlong` (darwin jni_md.h:61 `typedef long long jlong`; 64-bit everywhere).
pub(crate) type JLong = i64;
/// `jbyte` (jni_md.h:64 `typedef signed char jbyte`).
pub(crate) type JByte = i8;
/// `jboolean` (jni.h:57 `typedef unsigned char jboolean`).
pub(crate) type JBoolean = u8;

/// What a native method receives: `JNIEnv *`. One deref yields the
/// interface-struct pointer, which we index as a flat slot table.
pub(crate) type JniEnv = *mut *const *const c_void;

/// `FindClass` — slot 6 (Zulu 17 jni.h:225).
const SLOT_FIND_CLASS: usize = 6;
/// `ThrowNew` — slot 14 (Zulu 17 jni.h:246).
const SLOT_THROW_NEW: usize = 14;
/// `GetArrayLength` — slot 171 (Zulu 17 jni.h:625).
const SLOT_GET_ARRAY_LENGTH: usize = 171;
/// `NewByteArray` — slot 176 (Zulu 17 jni.h:637).
const SLOT_NEW_BYTE_ARRAY: usize = 176;
/// `NewLongArray` — slot 180 (Zulu 17 jni.h:645).
const SLOT_NEW_LONG_ARRAY: usize = 180;
/// `GetByteArrayRegion` — slot 200 (Zulu 17 jni.h:688).
const SLOT_GET_BYTE_ARRAY_REGION: usize = 200;
/// `SetByteArrayRegion` — slot 208 (Zulu 17 jni.h:705).
const SLOT_SET_BYTE_ARRAY_REGION: usize = 208;
/// `SetLongArrayRegion` — slot 212 (Zulu 17 jni.h:713).
const SLOT_SET_LONG_ARRAY_REGION: usize = 212;

type FindClassFn = unsafe extern "system" fn(JniEnv, *const c_char) -> JObject;
type ThrowNewFn = unsafe extern "system" fn(JniEnv, JObject, *const c_char) -> JInt;
type GetArrayLengthFn = unsafe extern "system" fn(JniEnv, JObject) -> JInt;
type NewByteArrayFn = unsafe extern "system" fn(JniEnv, JInt) -> JObject;
type NewLongArrayFn = unsafe extern "system" fn(JniEnv, JInt) -> JObject;
type GetByteArrayRegionFn = unsafe extern "system" fn(JniEnv, JObject, JInt, JInt, *mut JByte);
type SetByteArrayRegionFn = unsafe extern "system" fn(JniEnv, JObject, JInt, JInt, *const JByte);
type SetLongArrayRegionFn = unsafe extern "system" fn(JniEnv, JObject, JInt, JInt, *const JLong);

/// Fetch function-table slot `idx`.
///
/// # Safety
/// `env` must be the live `JNIEnv *` the VM passed to the current native
/// call, used on the calling thread within that call.
unsafe fn slot(env: JniEnv, idx: usize) -> *const c_void {
    unsafe { *(*env).add(idx) }
}

/// Copy a whole Java `byte[]` into a Rust `Vec`.
///
/// # Safety
/// `env` as in [`slot`]; `arr` must be a live, non-null `byte[]` reference
/// from the same call.
pub(crate) unsafe fn get_byte_array(env: JniEnv, arr: JObject) -> Vec<u8> {
    let len_fn: GetArrayLengthFn =
        unsafe { std::mem::transmute(slot(env, SLOT_GET_ARRAY_LENGTH)) };
    let n = unsafe { len_fn(env, arr) };
    if n <= 0 {
        return Vec::new();
    }
    let mut v = vec![0u8; n as usize];
    let get_fn: GetByteArrayRegionFn =
        unsafe { std::mem::transmute(slot(env, SLOT_GET_BYTE_ARRAY_REGION)) };
    unsafe { get_fn(env, arr, 0, n, v.as_mut_ptr().cast::<JByte>()) };
    v
}

/// Build a new Java `byte[]` holding `data`. Null means the VM failed to
/// allocate — it then has a pending `OutOfMemoryError` which the JVM
/// throws when the native method returns, so null is simply passed up.
///
/// # Safety
/// `env` as in [`slot`].
pub(crate) unsafe fn new_byte_array(env: JniEnv, data: &[u8]) -> JObject {
    let new_fn: NewByteArrayFn = unsafe { std::mem::transmute(slot(env, SLOT_NEW_BYTE_ARRAY)) };
    let arr = unsafe { new_fn(env, data.len() as JInt) };
    if arr.is_null() || data.is_empty() {
        return arr;
    }
    let set_fn: SetByteArrayRegionFn =
        unsafe { std::mem::transmute(slot(env, SLOT_SET_BYTE_ARRAY_REGION)) };
    unsafe { set_fn(env, arr, 0, data.len() as JInt, data.as_ptr().cast::<JByte>()) };
    arr
}

/// Build a new Java `long[]` holding `data`. Null means the VM failed to
/// allocate, exactly as in [`new_byte_array`] — the pending
/// `OutOfMemoryError` throws when the native method returns.
///
/// # Safety
/// `env` as in [`slot`].
pub(crate) unsafe fn new_long_array(env: JniEnv, data: &[JLong]) -> JObject {
    let new_fn: NewLongArrayFn = unsafe { std::mem::transmute(slot(env, SLOT_NEW_LONG_ARRAY)) };
    let arr = unsafe { new_fn(env, data.len() as JInt) };
    if arr.is_null() || data.is_empty() {
        return arr;
    }
    let set_fn: SetLongArrayRegionFn =
        unsafe { std::mem::transmute(slot(env, SLOT_SET_LONG_ARRAY_REGION)) };
    unsafe { set_fn(env, arr, 0, data.len() as JInt, data.as_ptr()) };
    arr
}

/// Stage a `class_name(msg)` exception on the current thread; the JVM raises
/// it when the native method returns. `class_name` is a JNI internal name
/// (slashes, e.g. `jp/golia/kevy/ScalarGetSignal`), both NUL-terminated. If
/// the class can't be resolved, `FindClass` leaves its own error pending — a
/// thrown exception either way. After this call, make no further JNI calls
/// but return: a pending exception must not be shadowed by another JNI op.
///
/// # Safety
/// `env` as in [`slot`]; call at most once per native invocation.
pub(crate) unsafe fn throw(env: JniEnv, class_name: &CStr, msg: &CStr) {
    let find_fn: FindClassFn = unsafe { std::mem::transmute(slot(env, SLOT_FIND_CLASS)) };
    let class = unsafe { find_fn(env, class_name.as_ptr()) };
    if class.is_null() {
        return; // FindClass left a NoClassDefFoundError pending
    }
    let throw_fn: ThrowNewFn = unsafe { std::mem::transmute(slot(env, SLOT_THROW_NEW)) };
    unsafe { throw_fn(env, class, msg.as_ptr()) };
}
