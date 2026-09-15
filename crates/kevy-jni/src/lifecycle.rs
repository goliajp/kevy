//! Opening, closing and shutting down a store — the handle's whole life.

// `catch_unwind` at the ABI boundary. Its `Err` is the panic payload,
// and the point of catching it here is that a panic must not cross
// into C — see `boundary/no-panic-across-abi`. There is no Rust
// frame above this to hand it to, and the callee has already
// reported through its own error channel.
#![expect(
    clippy::let_underscore_must_use,
    reason = "catch_unwind exists to stop the unwind, not to report it"
)]

use crate::env::*;
use crate::{db_ptr, handle};
use kevy_ffi::KevyOpenOptions;
use std::panic::{AssertUnwindSafe, catch_unwind};

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
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let d = unsafe { get_byte_array(env, dir) };
        // SAFETY: `env` is this call's, `arr` is the live array reference JNI passed, and
        // the buffer is a local sized to the length just read from that array.
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

/// Clamp a `long[]` options field to `u64` (negatives are nonsense on every
/// knob; they clamp to 0, matching the N-API gate's boundary).
fn nz(v: JLong) -> u64 {
    if v < 0 { 0 } else { v as u64 }
}

/// `KevyNative.openWith(byte[] dir, long[] opts)` — open with explicit
/// durability/rewrite policy: durable at the UTF-8 path in `dir`, in-memory
/// when `dir` is null. `opts` is a `long[6]` (the simplest hand-JNI-safe
/// shape — one `GetArrayLength` + one `GetLongArrayRegion`), laid out as:
///
/// | index | field                                                     |
/// |------:|-----------------------------------------------------------|
/// |     0 | fsync — 0 everysec, 1 always, 2 no                         |
/// |     1 | shards — keyspace shards (0 = engine default)              |
/// |     2 | rewrite_pct — growth trigger, percent (0 = rule off)       |
/// |     3 | rewrite_min_size — growth rule's minimum size gate, bytes  |
/// |     4 | rewrite_bytes — absolute-size trigger (0 = off)            |
/// |     5 | rewrite_interval_secs — staleness trigger (0 = off)        |
///
/// Every slot must be filled — a `long[]` has no "missing field", so the
/// per-field defaults live one floor up (the Kotlin options class). A null
/// `opts` means exactly `open`'s defaults. Returns the db handle; 0 on
/// failure, including a wrong-length array.
///
/// # Safety
/// Called by the JVM only: `env` must be the current thread's `JNIEnv *`,
/// `dir` / `opts` live references (or null) from the same call.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_openWith")]
pub unsafe extern "system" fn jni_open_with(
    env: JniEnv,
    _class: JObject,
    dir: JObject,
    opts: JObject,
) -> JLong {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let d = if dir.is_null() { Vec::new() } else { unsafe { get_byte_array(env, dir) } };
        let o = if opts.is_null() {
            None
        } else {
            // SAFETY: `env` is the JNI env for this call and every argument is a live local —
            // see the module note.
            let raw = unsafe { get_long_array(env, opts) };
            if raw.len() != 6 {
                return 0;
            }
            Some(KevyOpenOptions {
                fsync: nz(raw[0]) as u8,
                shards: nz(raw[1]) as u32,
                rewrite_pct: nz(raw[2]) as u32,
                rewrite_min_size: nz(raw[3]),
                rewrite_bytes: nz(raw[4]),
                rewrite_interval_secs: nz(raw[5]),
            })
        };
        let opts_ptr = o.as_ref().map_or(std::ptr::null(), |o| o as *const KevyOpenOptions);
        let (ptr, len) = if dir.is_null() { (std::ptr::null(), 0) } else { (d.as_ptr(), d.len()) };
        // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
        // local still in scope, which is what the callee's `# Safety` requires.
        handle(unsafe { kevy_ffi::kevy_open_with(ptr, len, opts_ptr) })
    }))
    .unwrap_or(0)
}

/// `KevyNative.shutdown(long db)` — flush every shard's AOF (a REAL fsync),
/// write the feed continuity marker, then refuse every later write; reads
/// stay available, so the handle stays live (`close` it separately).
/// Idempotent — the deterministic teardown for a host's signal handler:
/// `KevyNative.shutdown(db); System.exit(0)`. Returns 0 on success, -1 on
/// misuse, -2 on an I/O failure (the store is still usable; retry).
///
/// # Safety
/// Called by the JVM only; `db` must be a live handle (or 0).
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_shutdown")]
pub unsafe extern "system" fn jni_shutdown(_env: JniEnv, _class: JObject, db: JLong) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 {
            return -1;
        }
        // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
        // local still in scope, which is what the callee's `# Safety` requires.
        unsafe { kevy_ffi::kevy_shutdown(db_ptr(db)) }
    }))
    .unwrap_or(-2)
}

/// `KevyNative.close(long db)` — close a store handle. 0 is a no-op; the
/// handle must not be used afterwards.
///
/// # Safety
/// Called by the JVM only; `db` must be a live handle from this library,
/// passed exactly once.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_close")]
pub unsafe extern "system" fn jni_close(_env: JniEnv, _class: JObject, db: JLong) {
    // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
    // local still in scope, which is what the callee's `# Safety` requires.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { kevy_ffi::kevy_close(db_ptr(db)) }));
}
