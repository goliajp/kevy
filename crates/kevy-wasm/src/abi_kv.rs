//! The KV + TTL data plane: string values, expiry, counters, keyspace
//! scans. Every write that succeeds also records its AOF frame (when
//! capture is on) in the exact byte shape a native kevy AOF carries, so
//! the host-pumped log replays anywhere.

use std::time::Duration;

use crate::{BAD_HANDLE, ERR, OK, arg, with};

/// `SET key value`. Returns 0, or -1 with a message in the result buffer.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_set(h: u32, kp: *const u8, kl: u32, vp: *const u8, vl: u32) -> i32 {
    // SAFETY: loader-staged argument buffers, live for this call.
    let (key, value) = unsafe { (arg(kp, kl), arg(vp, vl)) };
    with(h, BAD_HANDLE, |inst| match inst.store.set(key, value) {
        Ok(_) => {
            inst.log_frame(&[b"SET", key, value]);
            OK
        }
        Err(e) => inst.fail(e),
    })
}

/// `SET key value PX ttl_ms`. The AOF frame records an absolute
/// `PEXPIREAT` deadline so the TTL survives a reload unchanged.
/// Feed [`crate::abi_core::kevy_set_clock`] first.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
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
            Err(e) => inst.fail(e),
        }
    })
}

/// `GET key`. Returns 1 with the value in the result buffer, 0 on a
/// miss (absent or expired), or an error status.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_get(h: u32, kp: *const u8, kl: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    with(h, BAD_HANDLE, |inst| match inst.store.get(key) {
        Ok(Some(v)) => {
            inst.put_out(&v);
            1
        }
        Ok(None) => 0,
        Err(e) => inst.fail(e),
    })
}

/// `DEL key`. Returns 1 if the key existed, else 0.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
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
        Err(e) => inst.fail(e),
    })
}

/// `EXISTS key`. Returns 1 / 0.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_exists(h: u32, kp: *const u8, kl: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    with(h, BAD_HANDLE, |inst| match inst.store.exists(&[key]) {
        Ok(n) => n as i32,
        Err(e) => inst.fail(e),
    })
}

/// `PEXPIRE key ttl_ms` (logged as an absolute `PEXPIREAT`). Returns 1
/// if a live key was touched, else 0.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_expire(h: u32, kp: *const u8, kl: u32, ttl_ms: f64) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let key = unsafe { arg(kp, kl) };
    let ms = if ttl_ms > 0.0 { ttl_ms as u64 } else { 0 };
    with(h, BAD_HANDLE, |inst| {
        match inst.store.expire(key, Duration::from_millis(ms)) {
            Ok(true) => {
                let deadline = kevy_store::now_unix_ms().saturating_add(ms);
                inst.log_frame(&[b"PEXPIREAT", key, deadline.to_string().as_bytes()]);
                1
            }
            Ok(false) => 0,
            Err(e) => inst.fail(e),
        }
    })
}

/// `PERSIST key` — clear the TTL. Returns 1 if a TTL was removed, else 0.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
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
        Err(e) => inst.fail(e),
    })
}

/// `PTTL key`. Remaining TTL in ms; `-1` = no TTL, `-2` = no key,
/// `NaN` = bad handle.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
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
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
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
        Err(e) => inst.fail(e),
    })
}

/// `DBSIZE` — live key count (`NaN` for a bad handle).
#[unsafe(no_mangle)]
pub extern "C" fn kevy_dbsize(h: u32) -> f64 {
    with(h, f64::NAN, |inst| inst.store.dbsize() as f64)
}

/// `FLUSHALL` — wipe the keyspace.
#[unsafe(no_mangle)]
pub extern "C" fn kevy_flushall(h: u32) -> i32 {
    with(h, BAD_HANDLE, |inst| match inst.store.flushall() {
        Ok(()) => {
            inst.log_frame(&[b"FLUSHALL"]);
            OK
        }
        Err(e) => inst.fail(e),
    })
}

/// `KEYS pattern` / `SCAN`-style listing. `pattern` empty = every key;
/// `limit` 0 = unlimited. Returns the key count; the result buffer holds
/// each key as a little-endian `u32` length followed by the bytes.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_keys(h: u32, pp: *const u8, pl: u32, limit: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let pat = unsafe { arg(pp, pl) };
    let pattern = (!pat.is_empty()).then_some(pat);
    let limit = (limit > 0).then_some(limit as usize);
    with(h, BAD_HANDLE, |inst| {
        let keys = inst.store.collect_keys(pattern, limit);
        if keys.len() > i32::MAX as usize {
            return ERR;
        }
        inst.out.clear();
        for k in &keys {
            inst.out.extend_from_slice(&(k.len() as u32).to_le_bytes());
            inst.out.extend_from_slice(k);
        }
        keys.len() as i32
    })
}

/// `MGET key…` — read many keys in ONE crossing.
///
/// The per-call cost of a wasm KV read is dominated not by the lookup but
/// by the boundary: encoding the key into linear memory, the call itself,
/// and copying the value back out. For a small value that crossing costs
/// more than the lookup. Batching amortizes it across `count` keys — it
/// does not remove it, so a single small read still loses to a native
/// synchronous `localStorage.getItem`, which crosses nothing.
///
/// Argument buffer: `count` entries of `[len: u32 LE][key bytes]`.
/// Result buffer: `count` entries of `[len: u32 LE][value bytes]`, where
/// `len == u32::MAX` marks a miss (absent or expired) and is followed by
/// no bytes. Returns the entry count, or an error status.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_mget(h: u32, kp: *const u8, kl: u32, count: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let buf = unsafe { arg(kp, kl) };
    if count > i32::MAX as u32 {
        return ERR;
    }
    with(h, BAD_HANDLE, |inst| {
        inst.out.clear();
        let mut off = 0usize;
        for _ in 0..count {
            // Each entry: a u32 length header, then that many key bytes.
            // A truncated buffer is a caller bug, not a miss — fail loudly.
            let Some(hdr) = buf.get(off..off + 4) else { return ERR };
            let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
            off += 4;
            let Some(key) = buf.get(off..off + len) else { return ERR };
            off += len;
            match inst.store.get(key) {
                Ok(Some(v)) => {
                    inst.out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    inst.out.extend_from_slice(&v);
                }
                // The miss sentinel: no value bytes follow.
                Ok(None) => inst.out.extend_from_slice(&u32::MAX.to_le_bytes()),
                Err(e) => return inst.fail(e),
            }
        }
        count as i32
    })
}
