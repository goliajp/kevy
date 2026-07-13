//! N-API gate for kevy — the Node.js door onto the same engine.
//!
//! A thin shell over `kevy-ffi` (linked as a plain Rust rlib, not over the
//! C ABI): the addon exports `napi_register_module_v1`, which Node calls on
//! `process.dlopen`, and registers the ten functions bindings/node/node.js
//! wraps into the same API bun:ffi serves under Bun. Two decisions keep it
//! thin, both inherited from the JNI gate:
//!
//! - **Buffers in, Buffer out.** The JS side packs argv into one flat
//!   Buffer (u32-LE length prefix per argument — [`unpack_argv`]'s format),
//!   so no array or string APIs are ever touched; see [`napi`] for the
//!   twelve-symbol surface.
//! - **Handles are externals.** `*mut KevyDb` / `*mut KevySub` travel as
//!   opaque externals with no finalizer — close is explicit, exactly like
//!   the bun:ffi door, and the JS wrapper nulls its reference after.
//!
//! Every entry point catches panics (unwinding into Node is UB) and
//! reports failure as a thrown JS `Error`; a *protocol* error (`-ERR …`)
//! is a successful reply, KevyError-as-value territory for resp.js.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

use kevy_ffi::{KevyBuf, KevyDb, KevySub, unpack_argv};

mod napi;
use napi::{
    NapiCallback, NapiCallbackInfo, NapiEnv, NapiValue, args, buffer_bytes, external_ptr,
    make_buffer, make_external, napi_create_function, napi_create_string_utf8, napi_create_uint32,
    napi_set_named_property, null, throw, undefined,
};

const fn empty_buf() -> KevyBuf {
    KevyBuf {
        ptr: null_mut(),
        len: 0,
        cap: 0,
    }
}

/// Copy a reply buffer into a fresh JS Buffer, then free the buffer.
/// An empty reply maps to JS `null`, matching bun.js's `takeReply`.
///
/// # Safety
/// `env` must be the current callback's env; `buf` exactly as returned by
/// a kevy-ffi call, consumed exactly once.
unsafe fn take_buf(env: NapiEnv, buf: KevyBuf) -> NapiValue {
    if buf.len == 0 {
        return unsafe { null(env) };
    }
    let s = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
    let v = unsafe { make_buffer(env, s) };
    unsafe { kevy_ffi::kevy_buf_free(buf.ptr, buf.len, buf.cap) };
    v
}

/// `open(dirBuffer)` — open a persistent store rooted at the UTF-8 path.
unsafe extern "C" fn js_open(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [dir] = unsafe { args::<1>(env, info) };
        let Some(d) = (unsafe { buffer_bytes(env, dir) }) else {
            return unsafe { throw(env, "kevy: open needs a Buffer path\0") };
        };
        let db = unsafe { kevy_ffi::kevy_open(d.as_ptr(), d.len()) };
        if db.is_null() {
            return unsafe { throw(env, "kevy: open failed\0") };
        }
        unsafe { make_external(env, db) }
    }))
    .unwrap_or(null_mut())
}

/// `openMem()` — open a pure in-memory store.
unsafe extern "C" fn js_open_mem(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let db = kevy_ffi::kevy_open_mem();
        if db.is_null() {
            return unsafe { throw(env, "kevy: open failed\0") };
        }
        unsafe { make_external(env, db) }
    }))
    .unwrap_or(null_mut())
}

/// `close(db)` — close a store handle; the wrapper nulls its reference.
unsafe extern "C" fn js_close(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [ext] = unsafe { args::<1>(env, info) };
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        unsafe { kevy_ffi::kevy_close(db) };
        unsafe { undefined(env) }
    }))
    .unwrap_or(null_mut())
}

/// `cmd(db, packedArgv)` — run one command; the RESP-encoded reply comes
/// back as a Buffer (a protocol error is still a reply).
unsafe extern "C" fn js_cmd(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [ext, packed] = unsafe { args::<2>(env, info) };
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        let Some(bytes) = (unsafe { buffer_bytes(env, packed) }) else {
            return unsafe { throw(env, "kevy: cmd needs a packed-argv Buffer\0") };
        };
        let Some(argv) = unpack_argv(bytes) else {
            return unsafe { throw(env, "kevy: malformed packed argv\0") };
        };
        let ptrs: Vec<*const u8> = argv.iter().map(|a| a.as_ptr()).collect();
        let lens: Vec<usize> = argv.iter().map(Vec::len).collect();
        let mut out = empty_buf();
        let rc =
            unsafe { kevy_ffi::kevy_cmd(db, argv.len(), ptrs.as_ptr(), lens.as_ptr(), &mut out) };
        if rc != 0 {
            return unsafe { throw(env, "kevy: kevy_cmd misuse\0") };
        }
        unsafe { take_buf(env, out) }
    }))
    .unwrap_or(null_mut())
}

/// `subscribe(db, chanBuffer)` — polled subscription on one channel.
unsafe extern "C" fn js_subscribe(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    unsafe { sub_open(env, info, false) }
}

/// `psubscribe(db, patternBuffer)` — polled subscription on one glob pattern.
unsafe extern "C" fn js_psubscribe(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    unsafe { sub_open(env, info, true) }
}

unsafe fn sub_open(env: NapiEnv, info: NapiCallbackInfo, pattern: bool) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [ext, chan] = unsafe { args::<2>(env, info) };
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        let Some(c) = (unsafe { buffer_bytes(env, chan) }) else {
            return unsafe { throw(env, "kevy: subscribe needs a Buffer channel\0") };
        };
        let sub = if pattern {
            unsafe { kevy_ffi::kevy_psubscribe(db, c.as_ptr(), c.len()) }
        } else {
            unsafe { kevy_ffi::kevy_subscribe(db, c.as_ptr(), c.len()) }
        };
        if sub.is_null() {
            return unsafe { throw(env, "kevy: subscribe failed\0") };
        }
        unsafe { make_external(env, sub) }
    }))
    .unwrap_or(null_mut())
}

/// `subNext(sub)` — drain one pending pub/sub frame (a RESP-array Buffer),
/// or `undefined` when the queue is empty.
unsafe extern "C" fn js_sub_next(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [ext] = unsafe { args::<1>(env, info) };
        let sub: *mut KevySub = unsafe { external_ptr(env, ext) };
        let mut out = empty_buf();
        let rc = unsafe { kevy_ffi::kevy_sub_next(sub, &mut out) };
        match rc {
            1 => unsafe { take_buf(env, out) },
            0 => unsafe { undefined(env) },
            _ => unsafe { throw(env, "kevy: subscription misuse\0") },
        }
    }))
    .unwrap_or(null_mut())
}

/// `subClose(sub)` — close a subscription handle.
unsafe extern "C" fn js_sub_close(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [ext] = unsafe { args::<1>(env, info) };
        let sub: *mut KevySub = unsafe { external_ptr(env, ext) };
        unsafe { kevy_ffi::kevy_sub_close(sub) };
        unsafe { undefined(env) }
    }))
    .unwrap_or(null_mut())
}

/// `version()` — the engine version string.
unsafe extern "C" fn js_version(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let v = kevy_ffi::kevy_version();
        let len = unsafe { std::ffi::CStr::from_ptr(v) }.to_bytes().len();
        let mut out: NapiValue = null_mut();
        unsafe { napi_create_string_utf8(env, v, len, &mut out) };
        out
    }))
    .unwrap_or(null_mut())
}

/// `abi()` — the C ABI version ([`kevy_ffi::KEVY_ABI`]).
unsafe extern "C" fn js_abi(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let mut out: NapiValue = null_mut();
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
    const FNS: [(&str, NapiCallback); 10] = [
        ("open\0", js_open),
        ("openMem\0", js_open_mem),
        ("close\0", js_close),
        ("cmd\0", js_cmd),
        ("subscribe\0", js_subscribe),
        ("psubscribe\0", js_psubscribe),
        ("subNext\0", js_sub_next),
        ("subClose\0", js_sub_close),
        ("version\0", js_version),
        ("abi\0", js_abi),
    ];
    let _ = catch_unwind(AssertUnwindSafe(|| {
        for (name, cb) in FNS {
            let mut f: NapiValue = null_mut();
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
                unsafe { napi_set_named_property(env, exports, name.as_ptr().cast(), f) };
            }
        }
    }));
    exports
}
