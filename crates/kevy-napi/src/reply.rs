//! Turning a kevy-ffi reply buffer into a JS `Buffer`.
//!
//! Two lanes, two reclaimers: an owned reply is freed with `kevy_buf_free`,
//! a shared-lane reply with `kevy_buf_free_shared`. Crossing them would free
//! through the wrong allocator, which is why they never share a helper.

use crate::napi::*;
use kevy_ffi::KevyBuf;

/// Copy a reply buffer into a fresh JS Buffer, then free the buffer.
/// An empty reply maps to JS `null`, matching bun.js's `takeReply`.
///
/// # Safety
/// `env` must be the current callback's env; `buf` exactly as returned by
/// a kevy-ffi call, consumed exactly once.
pub(crate) unsafe fn take_buf(env: NapiEnv, buf: KevyBuf) -> NapiValue {
    if buf.len == 0 {
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        return unsafe { null(env) };
    }
    // SAFETY: `buf.ptr` and `buf.len` are the pair kevy-ffi just returned.
    let s = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
    // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
    let v = unsafe { make_buffer(env, s) };
    // SAFETY: the buffer came from kevy-ffi and is freed exactly once.
    unsafe { kevy_ffi::kevy_buf_free(buf.ptr, buf.len, buf.cap) };
    v
}

/// Copy a **shared-lane** reply buffer into a fresh JS Buffer, then free it
/// through the shared reclaimer. Mirrors [`take_buf`] but pairs with
/// [`kevy_ffi::kevy_buf_free_shared`], never [`kevy_ffi::kevy_buf_free`]: the
/// shared GET hands back a view whose `cap` is an OPAQUE owner handle (an `Arc`
/// raw pointer for a bulk value, a tagged `Vec` capacity for a small one), so
/// routing it through the plain free would corrupt the allocator — UB. The bytes
/// are still copied into the JS-owned Buffer; what the shared lane saves is the
/// engine-side clone into a fresh `Vec`.
///
/// Unlike [`take_buf`], a zero-length reply maps to an EMPTY Buffer, not `null`:
/// on this lane an empty value is still a hit (`rc == 1`); only `rc == 0` is a
/// miss, distinguished by the caller before this is reached.
///
/// # Safety
/// `env` must be the current callback's env; `buf` exactly as returned by
/// [`kevy_ffi::kevy_get_shared`], consumed exactly once.
pub(crate) unsafe fn take_buf_shared(env: NapiEnv, buf: KevyBuf) -> NapiValue {
    let s: &[u8] =
        // SAFETY: `buf.ptr` and `buf.len` are the pair kevy-ffi just returned.
        if buf.len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) } };
    // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
    let v = unsafe { make_buffer(env, s) };
    // SAFETY: the buffer came from kevy-ffi and is freed exactly once.
    unsafe { kevy_ffi::kevy_buf_free_shared(buf.ptr, buf.len, buf.cap) };
    v
}
