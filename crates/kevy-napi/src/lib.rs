//! N-API gate for kevy — the Node.js door onto the same engine.
//!
//! A thin shell over `kevy-ffi` (linked as a plain Rust rlib, not over the
//! C ABI): the addon exports `napi_register_module_v1`, which Node calls on
//! `process.dlopen`, and registers the thirteen functions bindings/node/node.js
//! wraps into the same API bun:ffi serves under Bun. Two decisions keep it
//! thin, both inherited from the JNI gate:
//!
//! - **Buffers in, Buffer out.** The JS side packs argv into one flat
//!   Buffer (u32-LE length prefix per argument — [`unpack_argv`]'s format),
//!   so no array or string APIs are ever touched; see [`napi`] for the
//!   thirteen-symbol surface.
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
    NapiCallback, NapiCallbackInfo, NapiEnv, NapiValue, args, buffer_bytes, external_ptr, get_i64,
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
unsafe fn take_buf_shared(env: NapiEnv, buf: KevyBuf) -> NapiValue {
    let s: &[u8] = if buf.len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) }
    };
    let v = unsafe { make_buffer(env, s) };
    unsafe { kevy_ffi::kevy_buf_free_shared(buf.ptr, buf.len, buf.cap) };
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

/// `get(db, keyBuffer)` — scalar fast-path GET: the raw value bytes as a
/// Buffer, or `null` on a miss.
///
/// Rides the **zero-copy shared lane** ([`kevy_ffi::kevy_get_shared`]): a bulk
/// value is an `Arc` refcount bump (no engine-side byte copy) whose bytes are
/// copied straight into the JS Buffer, freed via [`take_buf_shared`] — never
/// [`take_buf`], as the shared lane's `cap` is a tagged owner handle and mixing
/// frees is UB. This skips the malloc+memcpy the plain [`kevy_ffi::kevy_get`]
/// lane spends cloning the value into a fresh `Vec`, and — riding a raw key
/// rather than a packed argv — the whole RESP-framing floor `cmd()` pays.
///
/// An empty-string value is a hit (`rc == 1` → an EMPTY Buffer); only a miss is
/// `null` (`rc == 0`). A store error (GET on a non-string key — its only error
/// is `WrongType`, unrepresentable on this lane, `rc < 0`) throws a JS `Error`;
/// no custom signal class is needed (unlike the JNI gate) because index.js
/// wraps `getScalar` in an unconditional `catch` that re-runs the framed
/// `cmd(GET, key)`, surfacing the typed `-WRONGTYPE`.
///
/// # Safety
/// Called by Node only; `env` / `info` live for this call.
unsafe extern "C" fn js_get(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [ext, key] = unsafe { args::<2>(env, info) };
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        let Some(k) = (unsafe { buffer_bytes(env, key) }) else {
            return unsafe { throw(env, "kevy: get needs a Buffer key\0") };
        };
        let mut out = empty_buf();
        let rc = unsafe { kevy_ffi::kevy_get_shared(db, k.as_ptr(), k.len(), &mut out) };
        match rc {
            1 => unsafe { take_buf_shared(env, out) },
            0 => unsafe { null(env) },
            _ => unsafe { throw(env, "kevy: kevy_get_shared misuse\0") },
        }
    }))
    .unwrap_or(null_mut())
}

/// `set(db, keyBuffer, valBuffer, ttlMs)` — scalar fast-path SET (`ttlMs` 0 or
/// negative = no expiry). `undefined` on success; a storage error (or misuse)
/// throws. SET overwrites, so there is no WRONGTYPE hazard and no fallback.
///
/// # Safety
/// Called by Node only; `env` / `info` live for this call.
unsafe extern "C" fn js_set(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [ext, key, val, ttl] = unsafe { args::<4>(env, info) };
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        let (Some(k), Some(v)) =
            (unsafe { buffer_bytes(env, key) }, unsafe { buffer_bytes(env, val) })
        else {
            return unsafe { throw(env, "kevy: set needs Buffer key and value\0") };
        };
        let ttl_ms = unsafe { get_i64(env, ttl) };
        let ttl = if ttl_ms > 0 { ttl_ms as u64 } else { 0 };
        let rc =
            unsafe { kevy_ffi::kevy_set(db, k.as_ptr(), k.len(), v.as_ptr(), v.len(), ttl) };
        if rc < 0 {
            return unsafe { throw(env, "kevy: kevy_set misuse or storage error\0") };
        }
        unsafe { undefined(env) }
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
unsafe extern "C" fn js_sub_wait(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [ext, timeout] = unsafe { args::<2>(env, info) };
        let sub: *mut KevySub = unsafe { external_ptr(env, ext) };
        let timeout_ms = unsafe { get_i64(env, timeout) };
        let timeout_ms = if timeout_ms > 0 { timeout_ms as u64 } else { 0 };
        let mut out = empty_buf();
        let rc = unsafe { kevy_ffi::kevy_sub_wait(sub, timeout_ms, &mut out) };
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
    const FNS: [(&str, NapiCallback); 13] = [
        ("open\0", js_open),
        ("openMem\0", js_open_mem),
        ("close\0", js_close),
        ("cmd\0", js_cmd),
        ("get\0", js_get),
        ("set\0", js_set),
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
