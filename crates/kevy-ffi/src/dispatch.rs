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
