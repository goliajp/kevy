//! The three data entry points: one command, one get, one set.

use crate::napi::*;
use crate::{empty_buf, take_buf, take_buf_shared};
use kevy_ffi::{KevyDb, KevyOpenReport, unpack_argv};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

/// `cmd(db, packedArgv)` — run one command; the RESP-encoded reply comes
/// back as a Buffer (a protocol error is still a reply).
pub(crate) unsafe extern "C" fn js_cmd(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext, packed] = unsafe { args::<2>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        // SAFETY: `env` is this callback's; the value came from `args` on the same call.
        let Some(bytes) = (unsafe { buffer_bytes(env, packed) }) else {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: cmd needs a packed-argv Buffer\0") };
        };
        let Some(argv) = unpack_argv(bytes) else {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: malformed packed argv\0") };
        };
        let ptrs: Vec<*const u8> = argv.iter().map(|a| a.as_ptr()).collect();
        let lens: Vec<usize> = argv.iter().map(Vec::len).collect();
        let mut out = empty_buf();
        let rc =
            // SAFETY: the handle is a live external and each pointer/length pair is a local.
            unsafe { kevy_ffi::kevy_cmd(db, argv.len(), ptrs.as_ptr(), lens.as_ptr(), &mut out) };
        if rc != 0 {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: kevy_cmd misuse\0") };
        }
        // SAFETY: `env` is this callback's; `out` is the buffer kevy-ffi just filled and
        // this consumes it exactly once.
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
pub(crate) unsafe extern "C" fn js_get(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext, key] = unsafe { args::<2>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        // SAFETY: `env` is this callback's; the value came from `args` on the same call.
        let Some(k) = (unsafe { buffer_bytes(env, key) }) else {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: get needs a Buffer key\0") };
        };
        let mut out = empty_buf();
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        let rc = unsafe { kevy_ffi::kevy_get_shared(db, k.as_ptr(), k.len(), &mut out) };
        match rc {
            // SAFETY: `env` is this callback's; `out` is the buffer kevy-ffi just filled and
            // this consumes it exactly once.
            1 => unsafe { take_buf_shared(env, out) },
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            0 => unsafe { null(env) },
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
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
pub(crate) unsafe extern "C" fn js_set(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext, key, val, ttl] = unsafe { args::<4>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        let (Some(k), Some(v)) =
            // SAFETY: `env` is this callback's; the value came from `args` on the same call.
            // SAFETY: `env` is this callback's; the value came from `args` on the same call.
            (unsafe { buffer_bytes(env, key) }, unsafe { buffer_bytes(env, val) })
        else {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: set needs Buffer key and value\0") };
        };
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        let ttl_ms = unsafe { get_i64(env, ttl) };
        let ttl = if ttl_ms > 0 { ttl_ms as u64 } else { 0 };
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        let rc = unsafe { kevy_ffi::kevy_set(db, k.as_ptr(), k.len(), v.as_ptr(), v.len(), ttl) };
        if rc < 0 {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: kevy_set misuse or storage error\0") };
        }
        // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
        unsafe { undefined(env) }
    }))
    .unwrap_or(null_mut())
}

/// `openReport(db)` — the boot-replay verdict of this handle's open, as a
/// plain object `{ replayedCommands, replayedBytes, elapsedMs, droppedBytes,
/// corrupt, quarantineCount }`. `droppedBytes > 0` or `corrupt` means the
/// store recovered LESS than its files held (the dropped region was
/// quarantined next to the AOF): surface it as a startup health check.
///
/// Counts ride u64 → f64 (`corrupt` is a boolean): exact up to 2^53, far
/// past any real replay, and the shape JS arithmetic wants. Misuse throws.
///
/// # Safety
/// Called by Node only; `env` / `info` live for this call.
pub(crate) unsafe extern "C" fn js_open_report(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` and `info` are this callback's, which is what `args` requires.
        let [ext] = unsafe { args::<1>(env, info) };
        // SAFETY: `env` is this callback's; the external was made by `make_external`.
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        let mut rep = KevyOpenReport {
            replayed_commands: 0,
            replayed_bytes: 0,
            elapsed_ms: 0,
            dropped_bytes: 0,
            corrupt: 0,
            quarantine_count: 0,
        };
        // SAFETY: the handle is a live external and each pointer/length pair is a local.
        let rc = unsafe { kevy_ffi::kevy_open_report(db, &mut rep) };
        if rc != 0 {
            // SAFETY: `env` is the current callback's, which is fact 1 of the module note.
            return unsafe { throw(env, "kevy: kevy_open_report misuse\0") };
        }
        // SAFETY: `env` is this callback's and the out-parameters are live locals.
        let obj = unsafe { make_object(env) };
        // SAFETY: `env` is this callback's and every argument below is a live local —
        // facts 1 and 4 of the module note.
        unsafe {
            set_num(env, obj, "replayedCommands\0", rep.replayed_commands as f64);
            set_num(env, obj, "replayedBytes\0", rep.replayed_bytes as f64);
            set_num(env, obj, "elapsedMs\0", rep.elapsed_ms as f64);
            set_num(env, obj, "droppedBytes\0", rep.dropped_bytes as f64);
            set_bool(env, obj, "corrupt\0", rep.corrupt != 0);
            set_num(env, obj, "quarantineCount\0", f64::from(rep.quarantine_count));
        }
        obj
    }))
    .unwrap_or(null_mut())
}
