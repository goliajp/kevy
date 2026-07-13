//! Hand-written `node_api` access — no `napi` crate, the same discipline as
//! `kevy-sys` binding libc (and `kevy-jni` binding JNI) by hand.
//!
//! Unlike JNI's function table, N-API is a flat set of C symbols exported
//! by the node executable itself, so the whole binding is one `extern "C"`
//! block. The symbols resolve at load time (`build.rs` passes `-undefined
//! dynamic_lookup` on macOS; ELF shared objects allow it by default), and
//! everything declared here is N-API version 1 — stable since Node 8, far
//! below any Node this package supports.
//!
//! The gate keeps its surface to Buffers in / Buffer out plus opaque
//! externals for handles, which needs exactly the twelve symbols below.

use std::ffi::c_void;

/// Opaque environment pointer, valid for the current callback only.
pub(crate) type NapiEnv = *mut c_void;
/// Opaque JS value handle.
pub(crate) type NapiValue = *mut c_void;
/// Opaque callback-info handle passed to every native callback.
pub(crate) type NapiCallbackInfo = *mut c_void;
/// `napi_status` — 0 (`napi_ok`) is the only value this gate accepts.
pub(crate) type NapiStatus = i32;
/// A native function callable from JS.
pub(crate) type NapiCallback = unsafe extern "C" fn(NapiEnv, NapiCallbackInfo) -> NapiValue;
/// Finalizer for externals (unused: handles are closed explicitly).
pub(crate) type NapiFinalize = unsafe extern "C" fn(NapiEnv, *mut c_void, *mut c_void);

unsafe extern "C" {
    pub(crate) fn napi_get_cb_info(
        env: NapiEnv,
        cbinfo: NapiCallbackInfo,
        argc: *mut usize,
        argv: *mut NapiValue,
        this_arg: *mut NapiValue,
        data: *mut *mut c_void,
    ) -> NapiStatus;
    pub(crate) fn napi_get_buffer_info(
        env: NapiEnv,
        value: NapiValue,
        data: *mut *mut c_void,
        length: *mut usize,
    ) -> NapiStatus;
    pub(crate) fn napi_create_buffer_copy(
        env: NapiEnv,
        length: usize,
        data: *const c_void,
        result_data: *mut *mut c_void,
        result: *mut NapiValue,
    ) -> NapiStatus;
    pub(crate) fn napi_create_external(
        env: NapiEnv,
        data: *mut c_void,
        finalize_cb: Option<NapiFinalize>,
        finalize_hint: *mut c_void,
        result: *mut NapiValue,
    ) -> NapiStatus;
    pub(crate) fn napi_get_value_external(
        env: NapiEnv,
        value: NapiValue,
        result: *mut *mut c_void,
    ) -> NapiStatus;
    pub(crate) fn napi_create_function(
        env: NapiEnv,
        utf8name: *const std::ffi::c_char,
        length: usize,
        cb: NapiCallback,
        data: *mut c_void,
        result: *mut NapiValue,
    ) -> NapiStatus;
    pub(crate) fn napi_set_named_property(
        env: NapiEnv,
        object: NapiValue,
        utf8name: *const std::ffi::c_char,
        value: NapiValue,
    ) -> NapiStatus;
    pub(crate) fn napi_create_string_utf8(
        env: NapiEnv,
        str: *const std::ffi::c_char,
        length: usize,
        result: *mut NapiValue,
    ) -> NapiStatus;
    pub(crate) fn napi_create_uint32(
        env: NapiEnv,
        value: u32,
        result: *mut NapiValue,
    ) -> NapiStatus;
    pub(crate) fn napi_get_undefined(env: NapiEnv, result: *mut NapiValue) -> NapiStatus;
    pub(crate) fn napi_get_null(env: NapiEnv, result: *mut NapiValue) -> NapiStatus;
    pub(crate) fn napi_throw_error(
        env: NapiEnv,
        code: *const std::ffi::c_char,
        msg: *const std::ffi::c_char,
    ) -> NapiStatus;
}

/// Fetch up to `N` call arguments. Missing arguments come back as null
/// handles, which every consumer below treats as misuse.
///
/// # Safety
/// `env` / `info` must be the pair the runtime passed to the current
/// callback, used on the calling thread within that call.
pub(crate) unsafe fn args<const N: usize>(env: NapiEnv, info: NapiCallbackInfo) -> [NapiValue; N] {
    let mut argv = [std::ptr::null_mut(); N];
    let mut argc = N;
    let rc = unsafe {
        napi_get_cb_info(
            env,
            info,
            &mut argc,
            argv.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        argv = [std::ptr::null_mut(); N];
    }
    argv
}

/// Borrow a JS Buffer's bytes for the duration of the current callback.
/// `None` when the value is not a Buffer (or a null handle from [`args`]).
///
/// # Safety
/// `env` as in [`args`]; the returned slice must not outlive the callback.
pub(crate) unsafe fn buffer_bytes<'a>(env: NapiEnv, value: NapiValue) -> Option<&'a [u8]> {
    if value.is_null() {
        return None;
    }
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut len = 0usize;
    if unsafe { napi_get_buffer_info(env, value, &mut data, &mut len) } != 0 {
        return None;
    }
    if len == 0 {
        return Some(&[]);
    }
    Some(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) })
}

/// Copy `bytes` into a fresh JS Buffer. Null on allocation failure — the
/// runtime then has a pending exception, passed up as-is.
///
/// # Safety
/// `env` as in [`args`].
pub(crate) unsafe fn make_buffer(env: NapiEnv, bytes: &[u8]) -> NapiValue {
    let mut out: NapiValue = std::ptr::null_mut();
    let mut copy: *mut c_void = std::ptr::null_mut();
    let src = if bytes.is_empty() {
        std::ptr::NonNull::<u8>::dangling().as_ptr()
    } else {
        bytes.as_ptr().cast_mut()
    };
    let rc = unsafe { napi_create_buffer_copy(env, bytes.len(), src.cast(), &mut copy, &mut out) };
    if rc != 0 { std::ptr::null_mut() } else { out }
}

/// Wrap a raw pointer as an opaque external (no finalizer: handles are
/// closed explicitly, exactly like the bun:ffi door).
///
/// # Safety
/// `env` as in [`args`].
pub(crate) unsafe fn make_external<T>(env: NapiEnv, p: *mut T) -> NapiValue {
    let mut out: NapiValue = std::ptr::null_mut();
    let rc = unsafe { napi_create_external(env, p.cast(), None, std::ptr::null_mut(), &mut out) };
    if rc != 0 { std::ptr::null_mut() } else { out }
}

/// Unwrap an external back to its raw pointer; null on misuse.
///
/// # Safety
/// `env` as in [`args`].
pub(crate) unsafe fn external_ptr<T>(env: NapiEnv, value: NapiValue) -> *mut T {
    if value.is_null() {
        return std::ptr::null_mut();
    }
    let mut p: *mut c_void = std::ptr::null_mut();
    if unsafe { napi_get_value_external(env, value, &mut p) } != 0 {
        return std::ptr::null_mut();
    }
    p.cast()
}

/// `undefined`.
///
/// # Safety
/// `env` as in [`args`].
pub(crate) unsafe fn undefined(env: NapiEnv) -> NapiValue {
    let mut out: NapiValue = std::ptr::null_mut();
    unsafe { napi_get_undefined(env, &mut out) };
    out
}

/// `null`.
///
/// # Safety
/// `env` as in [`args`].
pub(crate) unsafe fn null(env: NapiEnv) -> NapiValue {
    let mut out: NapiValue = std::ptr::null_mut();
    unsafe { napi_get_null(env, &mut out) };
    out
}

/// Throw a JS `Error` with `msg` (a NUL-terminated literal) and return the
/// null value handle a throwing callback hands back.
///
/// # Safety
/// `env` as in [`args`]; `msg` must be NUL-terminated.
pub(crate) unsafe fn throw(env: NapiEnv, msg: &'static str) -> NapiValue {
    unsafe { napi_throw_error(env, std::ptr::null(), msg.as_ptr().cast()) };
    std::ptr::null_mut()
}
