//! The KV + TTL data plane: string values, expiry, counters, keyspace
//! scans. Every write that succeeds also records its AOF frame (when
//! capture is on) in the exact byte shape a native kevy AOF carries, so
//! the host-pumped log replays anywhere.
//!
//! ```
//! use kevy_wasm::abi_core::*;
//! use kevy_wasm::abi_kv::*;
//! let h = kevy_open(0);
//! // SAFETY: each pair points at that many readable bytes for the call.
//! unsafe {
//!     kevy_set(h, b"a".as_ptr(), 1, b"1".as_ptr(), 1);
//!     kevy_set(h, b"b".as_ptr(), 1, b"2".as_ptr(), 1);
//!     assert_eq!(kevy_del(h, b"a".as_ptr(), 1), 1);
//! }
//! assert_eq!(kevy_dbsize(h), 1.0);
//! kevy_close(h);
//! ```

use std::time::Duration;

use crate::{BAD_HANDLE, OK, arg, with};

pub use crate::abi_kv_multi::{kevy_keys, kevy_mget};

/// `SET key value`. Returns 0, or -1 with a message in the result buffer.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// # fn out(h: u32) -> Vec<u8> {
/// #     // SAFETY: the result buffer stays valid until the next call on `h`.
/// #     unsafe { std::slice::from_raw_parts(kevy_out_ptr(h), kevy_out_len(h) as usize) }.to_vec()
/// # }
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     assert_eq!(kevy_set(h, b"k".as_ptr(), 1, b"v1".as_ptr(), 2), 0);
///     assert_eq!(kevy_set(h, b"k".as_ptr(), 1, b"v2".as_ptr(), 2), 0); // overwrites
///     kevy_get(h, b"k".as_ptr(), 1);
/// }
/// assert_eq!(out(h), b"v2");
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_set(h: u32, kp: *const u8, kl: u32, vp: *const u8, vl: u32) -> i32 {
    // SAFETY: loader-staged argument buffers, live for this call.
    let (key, value) = unsafe { (arg(kp, kl), arg(vp, vl)) };
    with(h, BAD_HANDLE, |inst| match inst.store.set(key, value) {
        Ok(_) => {
            inst.log_frame(&[b"SET", key, value]);
            OK
        }
        Err(e) => inst.fail_kevy(&e),
    })
}

/// `SET key value PX ttl_ms`. The AOF frame records an absolute
/// `PEXPIREAT` deadline so the TTL survives a reload unchanged.
/// Feed [`crate::abi_core::kevy_set_clock`] first.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// let left = unsafe {
///     assert_eq!(kevy_set_ttl(h, b"s".as_ptr(), 1, b"tok".as_ptr(), 3, 30_000.0), 0);
///     kevy_pttl(h, b"s".as_ptr(), 1)
/// };
/// assert!(left > 0.0 && left <= 30_000.0);
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_set_ttl(
    h: u32,
    kp: *const u8,
    kl: u32,
    vp: *const u8,
    vl: u32,
    ttl_ms: f64,
) -> i32 {
    // SAFETY: loader-staged argument buffers, live for this call.
    let (key, value) = unsafe { (arg(kp, kl), arg(vp, vl)) };
    let ms = if ttl_ms > 0.0 { ttl_ms as u64 } else { 0 };
    with(h, BAD_HANDLE, |inst| {
        match inst.store.set_with_ttl(key, value, Duration::from_millis(ms)) {
            Ok(_) => {
                let deadline = kevy_store::now_unix_ms().saturating_add(ms);
                inst.log_frame(&[b"SET", key, value]);
                inst.log_frame(&[b"PEXPIREAT", key, deadline.to_string().as_bytes()]);
                OK
            }
            Err(e) => inst.fail_kevy(&e),
        }
    })
}

/// `GET key`. Returns 1 with the value in the result buffer, 0 on a
/// miss (absent or expired), or an error status.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// # fn out(h: u32) -> Vec<u8> {
/// #     // SAFETY: the result buffer stays valid until the next call on `h`.
/// #     unsafe { std::slice::from_raw_parts(kevy_out_ptr(h), kevy_out_len(h) as usize) }.to_vec()
/// # }
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     assert_eq!(kevy_get(h, b"k".as_ptr(), 1), 0); // miss
///     kevy_set(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1);
///     assert_eq!(kevy_get(h, b"k".as_ptr(), 1), 1);
/// }
/// assert_eq!(out(h), b"v");
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_get(h: u32, kp: *const u8, kl: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    with(h, BAD_HANDLE, |inst| match inst.store.get(key) {
        Ok(Some(v)) => {
            // `get` already allocated `v`; move it into the result buffer
            // rather than `put_out(&v)`'s `extend_from_slice` copy. Giving up
            // the reused `inst.out` allocation is free — it is overwritten on
            // the next call anyway. `kevy_out_ptr` / `kevy_out_len` read
            // `inst.out`, so they now view the moved vec.
            inst.out = v;
            1
        }
        Ok(None) => 0,
        Err(e) => inst.fail_kevy(&e),
    })
}

/// `DEL key`. Returns 1 if the key existed, else 0.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     kevy_set(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1);
///     assert_eq!(kevy_del(h, b"k".as_ptr(), 1), 1);
///     assert_eq!(kevy_del(h, b"k".as_ptr(), 1), 0); // already gone
/// }
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_del(h: u32, kp: *const u8, kl: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    with(h, BAD_HANDLE, |inst| match inst.store.del(&[key]) {
        Ok(n) => {
            if n > 0 {
                inst.log_frame(&[b"DEL", key]);
            }
            n as i32
        }
        Err(e) => inst.fail_kevy(&e),
    })
}

/// `EXISTS key`. Returns 1 / 0.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     assert_eq!(kevy_exists(h, b"k".as_ptr(), 1), 0);
///     kevy_set(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1);
///     assert_eq!(kevy_exists(h, b"k".as_ptr(), 1), 1);
/// }
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_exists(h: u32, kp: *const u8, kl: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    with(h, BAD_HANDLE, |inst| match inst.store.exists(&[key]) {
        Ok(n) => n as i32,
        Err(e) => inst.fail_kevy(&e),
    })
}

/// `PEXPIRE key ttl_ms` (logged as an absolute `PEXPIREAT`). Returns 1
/// if a live key was touched, else 0.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     assert_eq!(kevy_expire(h, b"k".as_ptr(), 1, 5_000.0), 0); // no such key
///     kevy_set(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1);
///     assert_eq!(kevy_expire(h, b"k".as_ptr(), 1, 5_000.0), 1);
///     assert!(kevy_pttl(h, b"k".as_ptr(), 1) > 0.0);
/// }
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_expire(h: u32, kp: *const u8, kl: u32, ttl_ms: f64) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    let ms = if ttl_ms > 0.0 { ttl_ms as u64 } else { 0 };
    with(h, BAD_HANDLE, |inst| match inst.store.expire(key, Duration::from_millis(ms)) {
        Ok(true) => {
            let deadline = kevy_store::now_unix_ms().saturating_add(ms);
            inst.log_frame(&[b"PEXPIREAT", key, deadline.to_string().as_bytes()]);
            1
        }
        Ok(false) => 0,
        Err(e) => inst.fail_kevy(&e),
    })
}

/// `PERSIST key` — clear the TTL. Returns 1 if a TTL was removed, else 0.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     kevy_set_ttl(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1, 5_000.0);
///     assert_eq!(kevy_persist(h, b"k".as_ptr(), 1), 1);
///     assert_eq!(kevy_pttl(h, b"k".as_ptr(), 1), -1.0); // no TTL any more
///     assert_eq!(kevy_persist(h, b"k".as_ptr(), 1), 0);
/// }
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_persist(h: u32, kp: *const u8, kl: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    with(h, BAD_HANDLE, |inst| match inst.store.persist(key) {
        Ok(true) => {
            inst.log_frame(&[b"PERSIST", key]);
            1
        }
        Ok(false) => 0,
        Err(e) => inst.fail_kevy(&e),
    })
}

/// `PTTL key`. Remaining TTL in ms; `-1` = no TTL, `-2` = no key,
/// `NaN` = bad handle.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     assert_eq!(kevy_pttl(h, b"k".as_ptr(), 1), -2.0); // no key
///     kevy_set(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1);
///     assert_eq!(kevy_pttl(h, b"k".as_ptr(), 1), -1.0); // no TTL
///     assert!(kevy_pttl(0, b"k".as_ptr(), 1).is_nan());
/// }
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_pttl(h: u32, kp: *const u8, kl: u32) -> f64 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    with(h, f64::NAN, |inst| inst.store.ttl_ms(key) as f64)
}

/// `INCRBY key delta` (`delta` may be negative). Returns 0 with the new
/// value as a decimal string in the result buffer, or an error status
/// (e.g. the value is not an integer).
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// # fn out(h: u32) -> Vec<u8> {
/// #     // SAFETY: the result buffer stays valid until the next call on `h`.
/// #     unsafe { std::slice::from_raw_parts(kevy_out_ptr(h), kevy_out_len(h) as usize) }.to_vec()
/// # }
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     kevy_incrby(h, b"n".as_ptr(), 1, 10.0);
///     assert_eq!(kevy_incrby(h, b"n".as_ptr(), 1, -3.0), 0);
///     assert_eq!(out(h), b"7");
///     kevy_set(h, b"s".as_ptr(), 1, b"abc".as_ptr(), 3);
///     assert_eq!(kevy_incrby(h, b"s".as_ptr(), 1, 1.0), -1); // not an integer
/// }
/// assert!(!out(h).is_empty()); // the error message
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_incrby(h: u32, kp: *const u8, kl: u32, delta: f64) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    let d = delta as i64;
    with(h, BAD_HANDLE, |inst| match inst.store.incr_by(key, d) {
        Ok(n) => {
            inst.log_frame(&[b"INCRBY", key, d.to_string().as_bytes()]);
            inst.put_out(n.to_string().as_bytes());
            OK
        }
        Err(e) => inst.fail_kevy(&e),
    })
}

/// `DBSIZE` — live key count (`NaN` for a bad handle).
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// assert_eq!(kevy_dbsize(h), 0.0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe { kevy_set(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
/// assert_eq!(kevy_dbsize(h), 1.0);
/// kevy_close(h);
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn kevy_dbsize(h: u32) -> f64 {
    with(h, f64::NAN, |inst| inst.store.dbsize() as f64)
}

/// `FLUSHALL` — wipe the keyspace.
///
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// let h = kevy_open(0);
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe { kevy_set(h, b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
/// assert_eq!(kevy_flushall(h), 0);
/// assert_eq!(kevy_dbsize(h), 0.0);
/// kevy_close(h);
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn kevy_flushall(h: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| match inst.store.flushall() {
        Ok(()) => {
            inst.log_frame(&[b"FLUSHALL"]);
            OK
        }
        Err(e) => inst.fail_kevy(&e),
    })
}
