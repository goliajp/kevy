//! Rust-only packed-argv dispatch for the byte-array bindings (JNI, N-API).
//!
//! These shells link kevy-ffi as a plain Rust rlib, not across the C ABI, so
//! they can hand the packed argv straight in — no need to marshal it into the
//! `argv`/`argv_len` pointer arrays [`crate::kevy_cmd`] takes for a real C
//! caller, then rebuild `Vec<Vec<u8>>` from those. `dispatch_packed` unpacks
//! once and calls `dispatch_argv` directly, dropping that ptr/len round-trip
//! and its second per-argument copy.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{KevyBuf, KevyDb, unpack_argv};

/// Execute one command from packed argv, in-process (no C ABI crossing).
///
/// `packed` is the u32-LE length-prefixed argv that [`unpack_argv`] decodes.
/// On success the RESP-encoded reply is written to `out` and 0 is returned —
/// a protocol-level error (`-ERR …`) is still a *successful* call with a RESP
/// error in `out`. Returns -1 on misuse (null `db`, or packed argv that is
/// truncated / empty) and -2 on an internal panic; `out` is the empty
/// sentinel on any non-zero return and must not be freed.
///
/// A Rust-side helper for the binding shells, not part of the C ABI.
///
/// # Safety
/// `db` must be a live handle from [`crate::kevy_open`] / [`crate::kevy_open_mem`].
pub unsafe fn dispatch_packed(db: *mut KevyDb, packed: &[u8], out: &mut KevyBuf) -> i32 {
    *out = KevyBuf::empty();
    if db.is_null() {
        return -1;
    }
    let Some(args) = unpack_argv(packed) else {
        return -1;
    };
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    let reply = catch_unwind(AssertUnwindSafe(|| {
        let mut buf = Vec::new();
        store.dispatch_argv(&args, &mut buf);
        buf
    }));
    match reply {
        Ok(buf) => {
            *out = KevyBuf::from_vec(buf);
            0
        }
        Err(_) => -2,
    }
}

/// The keys of a packed argv, borrowed from it: `None` when a length runs
/// past the end.
fn packed_keys(packed: &[u8]) -> Option<Vec<&[u8]>> {
    let mut keys = Vec::new();
    let mut rest = packed;
    while !rest.is_empty() {
        let (head, tail) = rest.split_first_chunk::<4>()?;
        let (key, tail) = tail.split_at_checked(u32::from_le_bytes(*head) as usize)?;
        keys.push(key);
        rest = tail;
    }
    Some(keys)
}

/// The length a missing key's slot carries in [`mget_packed`]'s reply.
pub const MGET_MISS: u32 = u32::MAX;

/// `MGET` for the byte-array bindings: `packed` holds the keys the way
/// [`unpack_argv`] reads an argv, and `out` gets one slot per key, in
/// order — a u32-LE length and the value's bytes, or [`MGET_MISS`] alone
/// for a key that is absent or not a string. The values are copied once,
/// from the store into `out`. Returns 0, -1 on misuse (null `db`, a
/// truncated key list) or -2 on a store error or panic; `out` is the empty
/// sentinel on any non-zero return and must not be freed.
///
/// A Rust-side helper for the binding shells, not part of the C ABI.
///
/// # Safety
/// `db` must be a live handle from [`crate::kevy_open`] / [`crate::kevy_open_mem`].
pub unsafe fn mget_packed(db: *mut KevyDb, packed: &[u8], out: &mut KevyBuf) -> i32 {
    *out = KevyBuf::empty();
    let Some(keys) = packed_keys(packed).filter(|_| !db.is_null()) else {
        return -1;
    };
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    let reply = catch_unwind(AssertUnwindSafe(|| {
        let mut buf = Vec::with_capacity(keys.len() * 8);
        store
            .mget_with(keys.iter().copied(), |v| match v {
                Some(v) => {
                    buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    buf.extend_from_slice(v);
                }
                None => buf.extend_from_slice(&MGET_MISS.to_le_bytes()),
            })
            .map(|()| buf)
    }));
    match reply {
        Ok(Ok(buf)) => {
            *out = KevyBuf::from_vec(buf);
            0
        }
        _ => -2,
    }
}

/// `MSET` for the byte-array bindings: `packed` holds key, value, key,
/// value… the way [`unpack_argv`] reads an argv, read in place. Returns 0,
/// -1 on misuse (null `db`, a truncated list, an odd count) or -2 on a
/// store error or panic.
///
/// A Rust-side helper for the binding shells, not part of the C ABI.
///
/// # Safety
/// `db` must be a live handle from [`crate::kevy_open`] / [`crate::kevy_open_mem`].
pub unsafe fn mset_packed(db: *mut KevyDb, packed: &[u8]) -> i32 {
    let Some(parts) = packed_keys(packed).filter(|p| !db.is_null() && p.len() % 2 == 0) else {
        return -1;
    };
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    let pairs: Vec<(&[u8], &[u8])> = parts.chunks(2).map(|kv| (kv[0], kv[1])).collect();
    match catch_unwind(AssertUnwindSafe(|| store.mset(&pairs))) {
        Ok(Ok(())) => 0,
        _ => -2,
    }
}

/// `GET` for the byte-array bindings, lending the value to `f` under the
/// shard's lock ([`kevy_embedded::Store::get_with`]) so a binding copies it
/// once, straight into its own array. `Err(-1)` on misuse (null `db`),
/// `Err(-2)` on a store error (the key holds another type) or a panic.
///
/// A Rust-side helper for the binding shells, not part of the C ABI.
///
/// # Safety
/// `db` must be a live handle from [`crate::kevy_open`] / [`crate::kevy_open_mem`].
pub unsafe fn get_lent<R>(
    db: *mut KevyDb,
    key: &[u8],
    f: impl FnOnce(Option<&[u8]>) -> R,
) -> Result<R, i32> {
    if db.is_null() {
        return Err(-1);
    }
    // SAFETY: checked non-null above; the contract requires a live `kevy_open*` handle.
    let store = unsafe { &(*db).store };
    match catch_unwind(AssertUnwindSafe(|| store.get_with(key, f))) {
        Ok(Ok(r)) => Ok(r),
        _ => Err(-2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(keys: &[&[u8]]) -> Vec<u8> {
        keys.iter()
            .flat_map(|k| (k.len() as u32).to_le_bytes().into_iter().chain(k.iter().copied()))
            .collect()
    }

    #[test]
    fn keys_are_read_in_place_and_a_cut_list_is_refused() {
        let p = pack(&[b"a", b"", b"long key"]);
        assert_eq!(packed_keys(&p), Some(vec![&b"a"[..], b"", b"long key"]));
        assert_eq!(packed_keys(&p[..p.len() - 1]), None);
        assert_eq!(packed_keys(&[]), Some(vec![]));
    }

    #[test]
    fn every_key_gets_its_slot_in_order() {
        let db = crate::kevy_open_mem();
        // SAFETY: `db` is the live handle just opened; every buffer is local.
        unsafe {
            let set = pack(&[b"SET", b"a", b"one"]);
            let lst = pack(&[b"RPUSH", b"l", b"x"]);
            for cmd in [&set, &lst] {
                let mut out = KevyBuf::empty();
                assert_eq!(dispatch_packed(db, cmd, &mut out), 0);
                crate::kevy_buf_free(out.ptr, out.len, out.cap);
            }
            let mut out = KevyBuf::empty();
            assert_eq!(mget_packed(db, &pack(&[b"a", b"missing", b"l", b"a"]), &mut out), 0);
            let got = std::slice::from_raw_parts(out.ptr, out.len).to_vec();
            crate::kevy_buf_free(out.ptr, out.len, out.cap);
            let miss = MGET_MISS.to_le_bytes();
            let hit = [&3u32.to_le_bytes()[..], b"one"].concat();
            assert_eq!(got, [&hit[..], &miss, &miss, &hit].concat(), "a list reads as a miss");
            assert_eq!(mget_packed(db, &[1, 0, 0], &mut out), -1);
            assert_eq!(mget_packed(std::ptr::null_mut(), &pack(&[b"a"]), &mut out), -1);
            crate::kevy_close(db);
        }
    }

    #[test]
    fn pairs_are_set_and_a_lone_key_is_refused() {
        let db = crate::kevy_open_mem();
        // SAFETY: `db` is the live handle just opened; every buffer is local.
        unsafe {
            assert_eq!(mset_packed(db, &pack(&[b"a", b"1", b"b", b"22"])), 0);
            assert_eq!(mset_packed(db, &pack(&[b"a", b"1", b"lonely"])), -1);
            let mut out = KevyBuf::empty();
            assert_eq!(mget_packed(db, &pack(&[b"b", b"a"]), &mut out), 0);
            let got = std::slice::from_raw_parts(out.ptr, out.len).to_vec();
            crate::kevy_buf_free(out.ptr, out.len, out.cap);
            let want = [&2u32.to_le_bytes()[..], b"22", &1u32.to_le_bytes(), b"1"].concat();
            assert_eq!(got, want);
            crate::kevy_close(db);
        }
    }
}
