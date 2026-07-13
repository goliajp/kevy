//! Hand-written `JNIEnv` access — no `jni` crate, the same discipline as
//! `kevy-sys` binding libc by hand.
//!
//! A native method receives `JNIEnv *env`, and `JNIEnv` is a pointer to
//! the VM's interface struct (`typedef const struct JNINativeInterface_
//! *JNIEnv;`, Zulu 17 jni.h:197). That struct is one flat table of
//! function pointers opening with four reserved slots (jni.h:214-219), so
//! indexing the table by slot number is the entire binding. The gate keeps
//! its JNI surface down to `byte[]` in / `byte[]` out, which needs exactly
//! four slots. Each index below was counted out of the real headers —
//! `reserved0` = slot 0, `GetVersion` = slot 4 — and the count is
//! identical in Zulu JDK 17 `include/jni.h` and Android NDK r27
//! `sysroot/usr/include/jni.h`:
//!
//! | slot | function             | declared at (Zulu 17 jni.h) |
//! |-----:|----------------------|-----------------------------|
//! |  171 | `GetArrayLength`     | line 625                    |
//! |  176 | `NewByteArray`       | line 637                    |
//! |  200 | `GetByteArrayRegion` | line 688                    |
//! |  208 | `SetByteArrayRegion` | line 705                    |

use std::ffi::c_void;

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

/// `GetArrayLength` — slot 171 (Zulu 17 jni.h:625).
const SLOT_GET_ARRAY_LENGTH: usize = 171;
/// `NewByteArray` — slot 176 (Zulu 17 jni.h:637).
const SLOT_NEW_BYTE_ARRAY: usize = 176;
/// `GetByteArrayRegion` — slot 200 (Zulu 17 jni.h:688).
const SLOT_GET_BYTE_ARRAY_REGION: usize = 200;
/// `SetByteArrayRegion` — slot 208 (Zulu 17 jni.h:705).
const SLOT_SET_BYTE_ARRAY_REGION: usize = 208;

type GetArrayLengthFn = unsafe extern "system" fn(JniEnv, JObject) -> JInt;
type NewByteArrayFn = unsafe extern "system" fn(JniEnv, JInt) -> JObject;
type GetByteArrayRegionFn = unsafe extern "system" fn(JniEnv, JObject, JInt, JInt, *mut JByte);
type SetByteArrayRegionFn = unsafe extern "system" fn(JniEnv, JObject, JInt, JInt, *const JByte);

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
