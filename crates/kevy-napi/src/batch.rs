//! Batch write door — `setMany`. Split out of `lib.rs` for the house
//! 500-LOC rule.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

use kevy_ffi::{KevyDb, unpack_argv};

use crate::napi::{
    NapiCallbackInfo, NapiEnv, NapiValue, args, buffer_bytes, external_ptr, throw, undefined,
};

/// `setMany(db, packedPairs)` — batch SET. `packedPairs` is a flat argv of
/// key/value pairs (`unpack_argv`'s u32-LE length-prefixed format,
/// `[k0,v0,k1,v1,…]`) so the whole batch crosses the addon boundary once; each
/// pair is applied via the scalar SET. `undefined` on success; an odd pair
/// count or a storage error throws. Durability is unchanged (each set appends
/// to the AOF).
///
/// # Safety
/// Called by Node only; `env` / `info` live for this call.
pub(crate) unsafe extern "C" fn js_set_many(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    catch_unwind(AssertUnwindSafe(|| {
        let [ext, packed] = unsafe { args::<2>(env, info) };
        let db: *mut KevyDb = unsafe { external_ptr(env, ext) };
        let Some(bytes) = (unsafe { buffer_bytes(env, packed) }) else {
            return unsafe { throw(env, "kevy: setMany needs a packed-pairs Buffer\0") };
        };
        let Some(pairs) = unpack_argv(bytes) else {
            return unsafe { throw(env, "kevy: malformed packed pairs\0") };
        };
        if pairs.len() % 2 != 0 {
            return unsafe { throw(env, "kevy: setMany needs key/value pairs\0") };
        }
        for kv in pairs.as_chunks::<2>().0 {
            let (k, v) = (&kv[0], &kv[1]);
            let rc = unsafe { kevy_ffi::kevy_set(db, k.as_ptr(), k.len(), v.as_ptr(), v.len(), 0) };
            if rc < 0 {
                return unsafe { throw(env, "kevy: setMany store error\0") };
            }
        }
        unsafe { undefined(env) }
    }))
    .unwrap_or(null_mut())
}
