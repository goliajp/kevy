//! Scalar (RESP-free) publish — the write-side pub/sub analog of `kevy_get`.
//!
//! `kevy_subscribe` has always been a direct entry point, but publishing had
//! to ride `kevy_cmd(["PUBLISH", chan, payload])`: argv marshalling, a
//! dispatch-table hop, and a RESP `:N\r\n` reply allocated, crossed, parsed,
//! and freed — all to carry one integer. This entry point calls the store's
//! publish directly and returns that integer. Split out of `lib.rs` for the
//! house 500-LOC rule; additive, `KEVY_ABI` unchanged.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::KevyDb;

/// `PUBLISH channel payload` without the RESP round-trip: delivers `payload`
/// to every in-process subscriber (direct + pattern) and returns the receiver
/// count — the `:N` a framed PUBLISH replies with — or -1 on misuse or a
/// poisoned store. No argv packing, no reply buffer to allocate or free.
///
/// # Safety
/// `chan` must point to `chan_len` readable bytes and `payload` to
/// `payload_len` readable bytes (`payload` may be null only when
/// `payload_len` is 0); `db` must be a live handle from `kevy_open*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_publish(
    db: *mut KevyDb,
    chan: *const u8,
    chan_len: usize,
    payload: *const u8,
    payload_len: usize,
) -> i64 {
    if db.is_null() || chan.is_null() || (payload.is_null() && payload_len != 0) {
        return -1;
    }
    let store = unsafe { &(*db).store };
    let channel = unsafe { std::slice::from_raw_parts(chan, chan_len) };
    let msg: &[u8] = if payload_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(payload, payload_len) }
    };
    match catch_unwind(AssertUnwindSafe(|| store.publish(channel, msg))) {
        Ok(count) => count as i64,
        Err(_) => -1,
    }
}
