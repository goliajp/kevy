//! Opening, closing and reporting on a store — the handle's whole life.

use crate::napi::*;
use kevy_ffi::{KevyDb, KevyOpenOptions};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

/// `open(dirBuffer)` — open a persistent store rooted at the UTF-8 path.
pub(crate) unsafe extern "C" fn js_open(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [dir] = unsafe { args::<1>(env, info) };
        // SAFETY: `env` is this callback's; the value came from `args` on the same call.
        let Some(d) = (unsafe { buffer_bytes(env, dir) }) else {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: open needs a Buffer path\0") };
        };
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        let db = unsafe { kevy_ffi::kevy_open(d.as_ptr(), d.len()) };
        if db.is_null() {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: open failed\0") };
        }
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        unsafe { make_external(env, db) }
    }))
    .unwrap_or(null_mut())
}

/// `openMem()` — open a pure in-memory store.
pub(crate) unsafe extern "C" fn js_open_mem(env: NapiEnv, _info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let db = kevy_ffi::kevy_open_mem();
        if db.is_null() {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: open failed\0") };
        }
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        unsafe { make_external(env, db) }
    }))
    .unwrap_or(null_mut())
}

/// Read `{fsync, shards, rewritePct, rewriteMinSize, rewriteBytes,
/// rewriteIntervalSecs}` off a plain options object into the C-ABI options
/// struct. Every field is optional: absent / non-number keeps the exact
/// default `kevy_open` uses. A non-object (`null` / `undefined` / a missing
/// argument) is `None` — "no options", a NULL `opts` on the ABI.
///
/// # Safety
/// `env` must be the current callback's env.
unsafe fn read_open_options(env: NapiEnv, v: NapiValue) -> Option<KevyOpenOptions> {
    // SAFETY: `env` is this callback's; the name is a NUL-terminated literal.
    if !unsafe { is_object(env, v) } {
        return None;
    }
    Some(KevyOpenOptions {
        // SAFETY: `env` is this callback's; the name is a NUL-terminated literal.
        fsync: unsafe { get_field_u64(env, v, "fsync\0", 0) } as u8,
        // SAFETY: `env` is this callback's; the name is a NUL-terminated literal.
        shards: unsafe { get_field_u64(env, v, "shards\0", 0) } as u32,
        // SAFETY: `env` is this callback's; the name is a NUL-terminated literal.
        rewrite_pct: unsafe { get_field_u64(env, v, "rewritePct\0", 100) } as u32,
        // SAFETY: `env` is this callback's; the name is a NUL-terminated literal.
        rewrite_min_size: unsafe { get_field_u64(env, v, "rewriteMinSize\0", 64 * 1024 * 1024) },
        // SAFETY: `env` is this callback's; the name is a NUL-terminated literal.
        rewrite_bytes: unsafe { get_field_u64(env, v, "rewriteBytes\0", 0) },
        // SAFETY: `env` is this callback's; the name is a NUL-terminated literal.
        rewrite_interval_secs: unsafe { get_field_u64(env, v, "rewriteIntervalSecs\0", 0) },
    })
}

/// `openWith(dirOrNull, optsOrNull)` — [`js_open`] with explicit
/// durability/rewrite policy: durable at the Buffer path when `dir` is one,
/// in-memory when it is `null`/`undefined`. `opts` is a plain object read by
/// [`read_open_options`]; `null` means `kevy_open`'s exact defaults.
pub(crate) unsafe extern "C" fn js_open_with(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [dir, opts] = unsafe { args::<2>(env, info) };
        // SAFETY: `env` is this callback's; the value came from `args` on the same call.
        let d = unsafe { buffer_bytes(env, dir) };
        // SAFETY: `env` is this callback's and `opts` came from `args` on the same call.
        let o = unsafe { read_open_options(env, opts) };
        let opts_ptr = o.as_ref().map_or(std::ptr::null(), |o| o as *const KevyOpenOptions);
        let (ptr, len) = d.map_or((std::ptr::null(), 0), |b| (b.as_ptr(), b.len()));
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        let db = unsafe { kevy_ffi::kevy_open_with(ptr, len, opts_ptr) };
        if db.is_null() {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: openWith failed\0") };
        }
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        unsafe { make_external(env, db) }
    }))
    .unwrap_or(null_mut())
}

/// `shutdown(db)` — flush every shard's AOF (a REAL fsync), write the feed
/// continuity marker, then refuse every later write; reads stay available,
/// so the handle stays live (close it separately). Idempotent — the
/// deterministic teardown for a signal handler. `undefined` on success;
/// misuse (-1) and an I/O failure (-2 — the store is still usable; retry
/// or exit) both throw.
pub(crate) unsafe extern "C" fn js_shutdown(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext] = unsafe { args::<1>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        match unsafe { kevy_ffi::kevy_shutdown(db) } {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            0 => unsafe { undefined(env) },
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            -1 => unsafe { throw(env, "kevy: kevy_shutdown misuse\0") },
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            _ => unsafe { throw(env, "kevy: shutdown I/O failure (store still usable; retry)\0") },
        }
    }))
    .unwrap_or(null_mut())
}

/// `close(db)` — close a store handle; the wrapper nulls its reference.
pub(crate) unsafe extern "C" fn js_close(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext] = unsafe { args::<1>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        unsafe { kevy_ffi::kevy_close(db) };
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        unsafe { undefined(env) }
    }))
    .unwrap_or(null_mut())
}
