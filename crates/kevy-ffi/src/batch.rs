//! Batch SET (RESP-free bulk write) — the amortized-write analog of
//! `kevy_set`.
//!
//! `kevy_set` crosses the FFI boundary once per key, so a bulk load (an
//! RDS on-ramp, "one row → many derived keys", a tight write loop) pays that
//! crossing N times — the cost a language binding cannot avoid per call.
//! `kevy_set_many` applies N sets in one crossing: N store inserts, AOF
//! appends buffered exactly as `kevy_set` does (the EverySec/Always policy
//! still governs the fsync — durability is unchanged). Split out of `lib.rs`
//! for the house 500-LOC rule; additive, `KEVY_ABI` unchanged.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::KevyDb;

/// Apply `n` SETs in a single call. `keys[i]` points at `key_lens[i]`
/// readable bytes and `vals[i]` at `val_lens[i]`. Returns 0 on success, -1 on
/// a null/misuse argument, -2 on a poisoned store or a per-op store error.
///
/// Durability is identical to `kevy_set` (each set appends to the AOF; the
/// EverySec/Always policy governs the fsync). The win is amortizing the
/// per-call boundary crossing a binding otherwise pays once per key.
///
/// # Safety
/// When `n > 0`, `keys`, `key_lens`, `vals`, `val_lens` must each point at `n`
/// readable elements; each `keys[i]` must point at `key_lens[i]` readable
/// bytes and each `vals[i]` at `val_lens[i]`. `db` must be a live handle from
/// `kevy_open*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_set_many(
    db: *mut KevyDb,
    n: usize,
    keys: *const *const u8,
    key_lens: *const usize,
    vals: *const *const u8,
    val_lens: *const usize,
) -> i32 {
    if db.is_null() {
        return -1;
    }
    if n == 0 {
        return 0;
    }
    if keys.is_null() || key_lens.is_null() || vals.is_null() || val_lens.is_null() {
        return -1;
    }
    let store = unsafe { &(*db).store };
    let done = catch_unwind(AssertUnwindSafe(|| {
        let keys = unsafe { std::slice::from_raw_parts(keys, n) };
        let key_lens = unsafe { std::slice::from_raw_parts(key_lens, n) };
        let vals = unsafe { std::slice::from_raw_parts(vals, n) };
        let val_lens = unsafe { std::slice::from_raw_parts(val_lens, n) };
        for i in 0..n {
            if keys[i].is_null() || vals[i].is_null() {
                return Err(());
            }
            let k = unsafe { std::slice::from_raw_parts(keys[i], key_lens[i]) };
            let v = unsafe { std::slice::from_raw_parts(vals[i], val_lens[i]) };
            store.set(k, v).map_err(|_| ())?;
        }
        Ok(())
    }));
    match done {
        Ok(Ok(())) => 0,
        Ok(Err(())) => -1,
        Err(_) => -2,
    }
}
