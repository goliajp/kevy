//! Multi-key reads of the KV plane: the keyspace listing and the batched
//! `MGET`. Split from `abi_kv` for the house 500-LOC rule; re-exported
//! there, so the public paths are unchanged.

use crate::{BAD_HANDLE, ERR, arg, with};

/// `KEYS pattern` / `SCAN`-style listing. `pattern` empty = every key;
/// `limit` 0 = unlimited. Returns the key count; the result buffer holds
/// each key as a little-endian `u32` length followed by the bytes.
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
///     for k in [&b"user:1"[..], b"user:2", b"cart:1"] {
///         kevy_set(h, k.as_ptr(), k.len() as u32, b"v".as_ptr(), 1);
///     }
///     assert_eq!(kevy_keys(h, b"cart:*".as_ptr(), 6, 0), 1);
/// }
/// assert_eq!(out(h), [&6u32.to_le_bytes()[..], b"cart:1"].concat());
/// // SAFETY: an empty pattern is `len == 0`, always accepted.
/// assert_eq!(unsafe { kevy_keys(h, std::ptr::null(), 0, 2) }, 2); // any keys, at most 2
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
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
/// ```
/// use kevy_wasm::abi_core::*;
/// use kevy_wasm::abi_kv::*;
/// # fn out(h: u32) -> Vec<u8> {
/// #     // SAFETY: the result buffer stays valid until the next call on `h`.
/// #     unsafe { std::slice::from_raw_parts(kevy_out_ptr(h), kevy_out_len(h) as usize) }.to_vec()
/// # }
/// let h = kevy_open(0);
/// let keys = [&1u32.to_le_bytes()[..], b"a", &1u32.to_le_bytes(), b"b"].concat();
/// // SAFETY: each pair points at that many readable bytes for the call.
/// unsafe {
///     kevy_set(h, b"a".as_ptr(), 1, b"one".as_ptr(), 3);
///     assert_eq!(kevy_mget(h, keys.as_ptr(), keys.len() as u32, 2), 2);
/// }
/// let miss = u32::MAX.to_le_bytes();
/// assert_eq!(out(h), [&3u32.to_le_bytes()[..], b"one", &miss].concat());
/// kevy_close(h);
/// ```
///
/// # Safety
///
/// Pointer/length pairs follow the crate's
/// [bytes-in convention](crate#abi-conventions).
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
            let Some(hdr) = off.checked_add(4).and_then(|e| buf.get(off..e)) else {
                return ERR;
            };
            let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
            off += 4;
            // See `unpack_argv`: on wasm32 this sum can overflow, and the
            // wrap only lands on the right answer by accident.
            let Some(key) = off.checked_add(len).and_then(|e| buf.get(off..e)) else {
                return ERR;
            };
            off += len;
            match inst.store.get(key) {
                Ok(Some(v)) => {
                    inst.out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    inst.out.extend_from_slice(&v);
                }
                // The miss sentinel: no value bytes follow.
                Ok(None) => inst.out.extend_from_slice(&u32::MAX.to_le_bytes()),
                Err(e) => return inst.fail_kevy(&e),
            }
        }
        count as i32
    })
}
