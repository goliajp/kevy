//! JNI gate for kevy — Android and desktop JVMs share this one cdylib.
//!
//! A thin shell over `kevy-ffi` (linked as a plain Rust rlib, not over the
//! C ABI): every export below is `Java_jp_golia_kevy_KevyNative_<name>`,
//! matching the `static native` methods of `jp.golia.kevy.KevyNative`
//! (bindings/android/java). Two decisions keep it thin:
//!
//! - **A handful of JNIEnv slots.** The Java side packs argv into one flat
//!   `byte[]` (u32-LE length prefix per argument), so the JNI surface is
//!   `byte[]` in / `byte[]` out (plus a `long[]` out for the open report
//!   and the scalar GET's signal exception) — see [`env`] for the
//!   hand-counted slot table.
//! - **Handles are `jlong`.** `*mut KevyDb` / `*mut KevySub` travel as
//!   opaque longs; 0 means failure, exactly like null on the C ABI.
//!
//! Every entry point catches panics (unwinding into the JVM is UB) and
//! reports failure through its normal channel: 0 for handles, null for
//! `byte[]`, negative for status ints. The `JNIEnv` pointer is only used
//! within the call that received it, as the JNI spec requires.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

use kevy_ffi::{KevyBuf, KevyDb, KevyOpenOptions, KevyOpenReport, KevySub, dispatch_packed};

mod env;
use env::{
    JBoolean, JInt, JLong, JObject, JniEnv, get_byte_array, get_long_array, new_byte_array,
    new_long_array, throw,
};

/// Rebuild a store pointer from the opaque handle Java carries.
fn db_ptr(handle: JLong) -> *mut KevyDb {
    handle as usize as *mut KevyDb
}

/// Rebuild a subscription pointer from its opaque handle.
fn sub_ptr(handle: JLong) -> *mut KevySub {
    handle as usize as *mut KevySub
}

/// Collapse a pointer into the opaque handle Java carries (0 = null).
fn handle<T>(p: *mut T) -> JLong {
    p as usize as JLong
}

const fn empty_buf() -> KevyBuf {
    KevyBuf { ptr: null_mut(), len: 0, cap: 0 }
}

/// Copy a reply buffer into a fresh `byte[]`, then free the buffer.
///
/// # Safety
/// `env` must be the current call's `JNIEnv *`; `buf` must be exactly as
/// returned by a kevy-ffi call, consumed exactly once.
unsafe fn take_buf(env: JniEnv, buf: KevyBuf) -> JObject {
    let arr = if buf.len == 0 {
        unsafe { new_byte_array(env, &[]) }
    } else {
        let s = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
        unsafe { new_byte_array(env, s) }
    };
    unsafe { kevy_ffi::kevy_buf_free(buf.ptr, buf.len, buf.cap) };
    arr
}

/// Copy a **shared-lane** reply buffer into a fresh `byte[]`, then free it
/// through the shared reclaimer. Mirrors [`take_buf`] but pairs with
/// [`kevy_ffi::kevy_buf_free_shared`], never [`kevy_ffi::kevy_buf_free`]:
/// the shared GET hands back a view whose `cap` is an OPAQUE owner handle (an
/// `Arc` raw pointer for bulk, a tagged `Vec` capacity for small), so routing
/// it through the plain free would corrupt the allocator — UB. The bytes are
/// still copied into the JVM-owned array (a `byte[]` must own its storage);
/// what the shared lane saves is the engine-side clone into a fresh `Vec`.
///
/// # Safety
/// `env` must be the current call's `JNIEnv *`; `buf` must be exactly as
/// returned by [`kevy_ffi::kevy_get_shared`], consumed exactly once.
unsafe fn take_buf_shared(env: JniEnv, buf: KevyBuf) -> JObject {
    let arr = if buf.len == 0 {
        unsafe { new_byte_array(env, &[]) }
    } else {
        let s = unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) };
        unsafe { new_byte_array(env, s) }
    };
    unsafe { kevy_ffi::kevy_buf_free_shared(buf.ptr, buf.len, buf.cap) };
    arr
}

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
        let d = unsafe { get_byte_array(env, dir) };
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
        let d = if dir.is_null() { Vec::new() } else { unsafe { get_byte_array(env, dir) } };
        let o = if opts.is_null() {
            None
        } else {
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
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { kevy_ffi::kevy_close(db_ptr(db)) }));
}

/// `KevyNative.cmd(long db, byte[] packedArgv)` — run one command; argv is
/// packed per [`kevy_ffi::unpack_argv`]. Returns the RESP-encoded reply (a
/// protocol error is still a reply), or null on misuse.
///
/// Goes through [`kevy_ffi::dispatch_packed`], the Rust-only lane: because
/// this crate links kevy-ffi as an rlib (not across the C ABI) the packed
/// bytes hand straight in — no `argv`/`argv_len` pointer arrays to build and
/// no second per-argument copy that crossing [`kevy_ffi::kevy_cmd`] would cost.
///
/// # Safety
/// Called by the JVM only: `env` / `packedArgv` live for this call, `db` a
/// live handle.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_cmd")]
pub unsafe extern "system" fn jni_cmd(
    env: JniEnv,
    _class: JObject,
    db: JLong,
    packed: JObject,
) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 || packed.is_null() {
            return null_mut();
        }
        let bytes = unsafe { get_byte_array(env, packed) };
        let mut out = empty_buf();
        let rc = unsafe { dispatch_packed(db_ptr(db), &bytes, &mut out) };
        if rc != 0 {
            return null_mut();
        }
        unsafe { take_buf(env, out) }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.get(long db, byte[] key)` — scalar fast-path GET: the raw
/// value bytes, or null on a miss (and on misuse).
///
/// Rides the **zero-copy shared lane** ([`kevy_ffi::kevy_get_shared`]): a bulk
/// value is an `Arc` refcount bump (no engine-side byte copy) whose bytes are
/// copied straight into the JVM array, freed via [`take_buf_shared`]. This
/// saves the malloc+memcpy that the plain [`kevy_ffi::kevy_get`] lane spends
/// cloning the value into a fresh `Vec` before handing it out.
///
/// The shared lane collapses a store error (GET on a non-string key — its only
/// error is `WrongType`) into `-2`, which `null` alone can't distinguish from a
/// miss (`0`). So on that error this throws a `jp.golia.kevy.ScalarGetSignal`,
/// telling the Java side to re-run the framed GET, which surfaces the proper
/// typed WRONGTYPE store exception (matching the remote backend). A miss stays
/// `null` — no framing cost on the common absent-key path.
///
/// # Safety
/// Called by the JVM only, same contract as [`jni_cmd`].
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_get")]
pub unsafe extern "system" fn jni_get(
    env: JniEnv,
    _class: JObject,
    db: JLong,
    key: JObject,
) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 || key.is_null() {
            return null_mut();
        }
        let k = unsafe { get_byte_array(env, key) };
        let mut out = empty_buf();
        let rc = unsafe { kevy_ffi::kevy_get_shared(db_ptr(db), k.as_ptr(), k.len(), &mut out) };
        match rc {
            1 => unsafe { take_buf_shared(env, out) },
            0 => null_mut(),
            // Store error (WrongType): `out` is the empty sentinel on a
            // non-hit, so there is no buffer to free — just signal the
            // framed-GET fallback and return.
            _ => {
                unsafe {
                    throw(
                        env,
                        c"jp/golia/kevy/ScalarGetSignal",
                        c"kevy scalar GET hit a store error; use the framed path",
                    );
                }
                null_mut()
            }
        }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.set(long db, byte[] key, byte[] val, long ttlMs)` — scalar
/// fast-path SET (`ttlMs` 0 or negative = no expiry). 0 on success,
/// negative on misuse.
///
/// # Safety
/// Called by the JVM only, same contract as [`jni_cmd`].
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_set")]
pub unsafe extern "system" fn jni_set(
    env: JniEnv,
    _class: JObject,
    db: JLong,
    key: JObject,
    val: JObject,
    ttl_ms: JLong,
) -> JInt {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 || key.is_null() || val.is_null() {
            return -1;
        }
        let k = unsafe { get_byte_array(env, key) };
        let v = unsafe { get_byte_array(env, val) };
        let ttl = if ttl_ms > 0 { ttl_ms as u64 } else { 0 };
        unsafe { kevy_ffi::kevy_set(db_ptr(db), k.as_ptr(), k.len(), v.as_ptr(), v.len(), ttl) }
    }))
    .unwrap_or(-2)
}

/// `KevyNative.openReport(long db)` — the boot-replay verdict of this
/// handle's open. A `long[6]` (the simplest hand-JNI-safe shape — no object
/// construction, one `NewLongArray` + one `SetLongArrayRegion`), laid out as:
///
/// | index | field                                                        |
/// |------:|--------------------------------------------------------------|
/// |     0 | replayed_commands — commands replayed from the AOF(s)         |
/// |     1 | replayed_bytes — bytes actually replayed (the valid prefixes) |
/// |     2 | elapsed_ms — wall-clock startup replay time                   |
/// |     3 | dropped_bytes — bytes dropped past the last replayable frame  |
/// |     4 | corrupt — 1 when any shard stopped at a corrupt frame, else 0 |
/// |     5 | quarantine_count — quarantine files the open's repair wrote   |
///
/// `[3] > 0` or `[4] != 0` means the store recovered LESS than its files
/// held (the dropped region was quarantined): a startup health check. Null
/// on misuse. Typed ergonomics (a data class) live one floor up.
///
/// # Safety
/// Called by the JVM only; `db` must be a live handle (or 0).
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_openReport")]
pub unsafe extern "system" fn jni_open_report(env: JniEnv, _class: JObject, db: JLong) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 {
            return null_mut();
        }
        let mut rep = KevyOpenReport {
            replayed_commands: 0,
            replayed_bytes: 0,
            elapsed_ms: 0,
            dropped_bytes: 0,
            corrupt: 0,
            quarantine_count: 0,
        };
        if unsafe { kevy_ffi::kevy_open_report(db_ptr(db), &mut rep) } != 0 {
            return null_mut();
        }
        let fields: [JLong; 6] = [
            rep.replayed_commands as JLong,
            rep.replayed_bytes as JLong,
            rep.elapsed_ms as JLong,
            rep.dropped_bytes as JLong,
            JLong::from(rep.corrupt),
            JLong::from(rep.quarantine_count),
        ];
        unsafe { new_long_array(env, &fields) }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.version()` — the engine version as UTF-8 bytes.
///
/// # Safety
/// Called by the JVM only.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_version")]
pub unsafe extern "system" fn jni_version(env: JniEnv, _class: JObject) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        let v = unsafe { std::ffi::CStr::from_ptr(kevy_ffi::kevy_version()) };
        unsafe { new_byte_array(env, v.to_bytes()) }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.subscribe(long db, byte[] chan, boolean pattern)` — open a
/// polled subscription on one channel (or glob pattern). Returns the
/// subscription handle, 0 on failure.
///
/// # Safety
/// Called by the JVM only, same contract as [`jni_cmd`].
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_subscribe")]
pub unsafe extern "system" fn jni_subscribe(
    env: JniEnv,
    _class: JObject,
    db: JLong,
    chan: JObject,
    pattern: JBoolean,
) -> JLong {
    catch_unwind(AssertUnwindSafe(|| {
        if db == 0 || chan.is_null() {
            return 0;
        }
        let c = unsafe { get_byte_array(env, chan) };
        let sub = if pattern != 0 {
            unsafe { kevy_ffi::kevy_psubscribe(db_ptr(db), c.as_ptr(), c.len()) }
        } else {
            unsafe { kevy_ffi::kevy_subscribe(db_ptr(db), c.as_ptr(), c.len()) }
        };
        handle(sub)
    }))
    .unwrap_or(0)
}

/// `KevyNative.subNext(long sub)` — drain one pending pub/sub frame,
/// encoded as the RESP array the server would push. Null when the queue is
/// empty (and on misuse).
///
/// # Safety
/// Called by the JVM only; `sub` must be a live subscription handle.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_subNext")]
pub unsafe extern "system" fn jni_sub_next(env: JniEnv, _class: JObject, sub: JLong) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        if sub == 0 {
            return null_mut();
        }
        let mut out = empty_buf();
        let rc = unsafe { kevy_ffi::kevy_sub_next(sub_ptr(sub), &mut out) };
        if rc == 1 { unsafe { take_buf(env, out) } } else { null_mut() }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.subWait(long sub, long timeoutMs)` — block up to `timeoutMs`
/// (0 = forever) for one frame, parking in the kernel instead of spinning.
/// Returns the RESP-array frame bytes, or null on timeout / bus-gone / misuse.
/// The blocking twin of [`jni_sub_next`]; lets a JVM subscriber wait without
/// a busy poll loop.
///
/// # Safety
/// Called by the JVM only; `sub` must be a live handle (or 0).
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_subWait")]
pub unsafe extern "system" fn jni_sub_wait(
    env: JniEnv,
    _class: JObject,
    sub: JLong,
    timeout_ms: JLong,
) -> JObject {
    catch_unwind(AssertUnwindSafe(|| {
        if sub == 0 {
            return null_mut();
        }
        let mut out = empty_buf();
        let rc = unsafe { kevy_ffi::kevy_sub_wait(sub_ptr(sub), timeout_ms as u64, &mut out) };
        if rc == 1 { unsafe { take_buf(env, out) } } else { null_mut() }
    }))
    .unwrap_or(null_mut())
}

/// `KevyNative.subClose(long sub)` — close a subscription handle. 0 is a
/// no-op; the handle must not be used afterwards.
///
/// # Safety
/// Called by the JVM only; `sub` must be a live handle, passed exactly once.
#[unsafe(export_name = "Java_jp_golia_kevy_KevyNative_subClose")]
pub unsafe extern "system" fn jni_sub_close(_env: JniEnv, _class: JObject, sub: JLong) {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe { kevy_ffi::kevy_sub_close(sub_ptr(sub)) }));
}
