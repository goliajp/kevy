//! N-API gate for kevy — the Node.js door onto the same engine.
//!
//! A thin shell over `kevy-ffi` (linked as a plain Rust rlib, not over the
//! C ABI): the addon exports `napi_register_module_v1`, which Node calls on
//! `process.dlopen`, and registers the sixteen functions bindings/node/node.js
//! wraps into the same API bun:ffi serves under Bun. Two decisions keep it
//! thin, both inherited from the JNI gate:
//!
//! - **Buffers in, Buffer out.** The JS side packs argv into one flat
//!   Buffer (u32-LE length prefix per argument — [`unpack_argv`]'s format),
//!   so no array or string APIs are ever touched (the open report's and the
//!   open options' plain objects are the exceptions); see [`napi`] for the
//!   eighteen-symbol surface.
//! - **Handles are externals.** `*mut KevyDb` / `*mut KevySub` travel as
//!   opaque externals with no finalizer — close is explicit, exactly like
//!   the bun:ffi door, and the JS wrapper nulls its reference after.
//!
//! Every entry point catches panics (unwinding into Node is UB) and
//! reports failure as a thrown JS `Error`; a *protocol* error (`-ERR …`)
//! is a successful reply, KevyError-as-value territory for resp.js.

//!
//! # Safety of the `unsafe` blocks below
//!
//! Every `unsafe` block in this file rests on one of four facts, stated here
//! rather than repeated at each call site:
//!
//! 1. **`env` is the current callback's.** N-API invokes each `js_*` function
//!    with a live `napi_env` and `napi_callback_info`; every helper that takes
//!    `env` is valid for exactly the duration of that call and no longer.
//! 2. **Handles are externals we made.** A `*mut KevyDb` / `*mut KevySub`
//!    reaches us only through `make_external`, and `external_ptr` hands the
//!    same pointer back. They are checked non-null before use, because JS can
//!    pass anything.
//! 3. **Buffer pairs come from kevy-ffi.** A `KevyBuf`'s `ptr`/`len`/`cap` are
//!    whatever the ffi call just wrote, and each is freed exactly once through
//!    the reclaimer that matches the lane it came from.
//! 4. **Pointer/length arguments are locals.** Every `as_ptr()` passed down is
//!    taken on a slice still in scope, with that slice's own `len()` beside it.
//!
//! Panics are caught at every entry point because unwinding into Node is
//! undefined behaviour; that is a correctness property of this file, not of
//! any one block.
// `catch_unwind` at the ABI boundary. Its `Err` is the panic payload,
// and the point of catching it here is that a panic must not cross
// into C — see `boundary/no-panic-across-abi`. There is no Rust
// frame above this to hand it to, and the callee has already
// reported through its own error channel.
#![expect(
    clippy::let_underscore_must_use,
    reason = "catch_unwind exists to stop the unwind, not to report it"
)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

use kevy_ffi::KevyBuf;

mod batch;
mod napi;
use napi::{
    NapiCallback, NapiCallbackInfo, NapiEnv, NapiValue, napi_create_function,
    napi_create_string_utf8, napi_create_uint32, napi_set_named_property,
};

const fn empty_buf() -> KevyBuf {
    KevyBuf { ptr: null_mut(), len: 0, cap: 0 }
}

mod kv;
mod lifecycle;
mod reply;
mod sub;

use kv::*;
use lifecycle::*;
pub(crate) use reply::{take_buf, take_buf_shared};
use sub::*;

/// `version()` — the engine version string.
unsafe extern "C" fn js_version(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let v = kevy_ffi::kevy_version();
        // SAFETY: the pointer addresses a NUL-terminated static that outlives the borrow.
        let len = unsafe { std::ffi::CStr::from_ptr(v) }.to_bytes().len();
        let mut out: NapiValue = null_mut();
        // SAFETY: `env` is this callback's and the out-parameters are live locals.
        unsafe { napi_create_string_utf8(env, v, len, &mut out) };
        out
    }))
    .unwrap_or(null_mut())
}

/// `abi()` — the C ABI version ([`kevy_ffi::KEVY_ABI`]).
unsafe extern "C" fn js_abi(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let mut out: NapiValue = null_mut();
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        unsafe { napi_create_uint32(env, kevy_ffi::kevy_abi(), &mut out) };
        out
    }))
    .unwrap_or(null_mut())
}

/// The N-API module entry point: Node resolves this symbol on
/// `process.dlopen` and calls it once to populate `exports`.
///
/// # Safety
/// Called by Node only; `env` / `exports` are live for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_register_module_v1(env: NapiEnv, exports: NapiValue) -> NapiValue {
    const FNS: [(&str, NapiCallback); 17] = [
        ("open\0", js_open),
        ("openMem\0", js_open_mem),
        ("openWith\0", js_open_with),
        ("shutdown\0", js_shutdown),
        ("close\0", js_close),
        ("cmd\0", js_cmd),
        ("get\0", js_get),
        ("set\0", js_set),
        ("setMany\0", batch::js_set_many),
        ("openReport\0", js_open_report),
        ("subscribe\0", js_subscribe),
        ("psubscribe\0", js_psubscribe),
        ("subNext\0", js_sub_next),
        ("subWait\0", js_sub_wait),
        ("subClose\0", js_sub_close),
        ("version\0", js_version),
        ("abi\0", js_abi),
    ];
    let _ = catch_unwind(AssertUnwindSafe(|| {
        for (name, cb) in FNS {
            let mut f: NapiValue = null_mut();
            // SAFETY: `env` is this callback's and every argument below is a live local —
            // facts 1 and 4 of the module note.
            let rc = unsafe {
                napi_create_function(
                    env,
                    name.as_ptr().cast(),
                    name.len() - 1,
                    cb,
                    null_mut(),
                    &mut f,
                )
            };
            if rc == 0 {
                // SAFETY: `env` is this callback's and the out-parameters are live locals.
                unsafe { napi_set_named_property(env, exports, name.as_ptr().cast(), f) };
            }
        }
    }));
    exports
}
