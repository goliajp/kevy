//! The three data entry points plus the open report.

use crate::env::*;
use crate::{db_ptr, empty_buf, take_buf, take_buf_shared};
use kevy_ffi::{KevyOpenReport, dispatch_packed};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

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
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let bytes = unsafe { get_byte_array(env, packed) };
        let mut out = empty_buf();
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let rc = unsafe { dispatch_packed(db_ptr(db), &bytes, &mut out) };
        if rc != 0 {
            return null_mut();
        }
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
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
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let k = unsafe { get_byte_array(env, key) };
        let mut out = empty_buf();
        // SAFETY: `env` is this call's, `arr` is the live array reference JNI passed, and
        // the buffer is a local sized to the length just read from that array.
        let rc = unsafe { kevy_ffi::kevy_get_shared(db_ptr(db), k.as_ptr(), k.len(), &mut out) };
        match rc {
            // SAFETY: `env` is the JNI env for this call and every argument is a live local —
            // see the module note.
            1 => unsafe { take_buf_shared(env, out) },
            0 => null_mut(),
            // Store error (WrongType): `out` is the empty sentinel on a
            // non-hit, so there is no buffer to free — just signal the
            // framed-GET fallback and return.
            _ => {
                // SAFETY: `env` is the JNI env for this call and every argument is a live local —
                // see the module note.
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
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let k = unsafe { get_byte_array(env, key) };
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        let v = unsafe { get_byte_array(env, val) };
        let ttl = if ttl_ms > 0 { ttl_ms as u64 } else { 0 };
        // SAFETY: `env` is this call's, `arr` is the live array reference JNI passed, and
        // the buffer is a local sized to the length just read from that array.
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
        // SAFETY: the handle came from `kevy_open*` and each pointer/length pair is a
        // local still in scope, which is what the callee's `# Safety` requires.
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
        // SAFETY: `env` is the JNI env for this call and every argument is a live local —
        // see the module note.
        unsafe { new_long_array(env, &fields) }
    }))
    .unwrap_or(null_mut())
}
