//! The raw command channel the client contract mandates for every port
//! (`docs/client-contract.md` §5.2 / §7: "every port MUST expose a raw
//! command channel `cmd(argv) -> Reply`"). The typed KV/TTL/pubsub/AOF
//! exports intentionally lag the full verb grammar; this is the escape
//! hatch that reaches whatever the compiled-in dispatcher owns an arm for.
//!
//! # Feature reach
//!
//! The wasm module is built `features = ["core", "persist"]`, so `cmd`
//! reaches every verb in that closure: the string/hash/list/set/zset,
//! bitmap, keyspace and misc surfaces. Index (`IDX.*` / `VIEW.*`) and
//! replication verbs are **not** compiled into this build and come back
//! as an `-ERR unknown command '…'` RESP error — the correct, expected
//! answer, not a bug.
//!
//! # Persistence note
//!
//! `cmd` runs through [`kevy_embedded::Store::dispatch_argv`] directly, so
//! writes issued this way are **not** mirrored into the host-mediated AOF
//! pump (the typed ops' `log_frame` path). A store that mixes `cmd` writes
//! with the durability pump will not see those writes in its replay log.
//! The typed KV surface remains the durable write path.

use crate::{BAD_HANDLE, ERR, Instance, arg, with};

/// Run an arbitrary verb and return its RESP reply.
///
/// The argument buffer is **packed argv**: each argument is a `u32`
/// little-endian length prefix followed by that many bytes, back to back
/// — the same layout [`crate::abi_kv::kevy_mget`] uses for keys and the
/// FFI byte-array bindings use for their commands. The RESP-encoded reply
/// lands in the instance result buffer (RESP2; the wasm build negotiates
/// no RESP3); the loader parses it into a `Reply`.
///
/// Returns the reply byte length (`>= 0`), [`ERR`] (`-1`) on a
/// malformed/empty packed argv (message in the result buffer), or
/// [`BAD_HANDLE`] (`-2`) for an unknown handle. A verb-level failure
/// (`-ERR …`, `WRONGTYPE …`) is a *successful* call whose reply bytes are
/// a RESP error frame — not a `-1` status.
///
/// # Safety
///
/// Pointer/length pairs follow the [`crate::arg`] contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kevy_cmd(h: u32, p: *const u8, l: u32) -> i32 {
    // SAFETY: loader-staged argument buffer, live for this call.
    let packed = unsafe { arg(p, l) };
    with(h, BAD_HANDLE, |inst| {
        let Some(argv) = unpack_argv(packed) else {
            return inst.fail("cmd: malformed or empty packed argv");
        };
        inst.out.clear();
        // Disjoint field borrows: `store` reads, `out` is written.
        let Instance { store, out, .. } = inst;
        store.dispatch_argv(&argv, out);
        if out.len() > i32::MAX as usize {
            return ERR;
        }
        out.len() as i32
    })
}

/// Decode packed argv — a `u32`-LE length prefix then that many bytes,
/// repeated back to back. Returns `None` on a truncated prefix/body or an
/// empty argv (caller misuse, not a protocol error). Mirrors
/// `kevy_ffi::unpack_argv`; kept local so the wasm closure takes no
/// dependency on the C-ABI crate.
fn unpack_argv(packed: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut args = Vec::new();
    let mut pos = 0usize;
    while pos < packed.len() {
        let head = packed.get(pos..pos + 4)?;
        let len = u32::from_le_bytes(head.try_into().ok()?) as usize;
        pos += 4;
        let body = packed.get(pos..pos + len)?;
        args.push(body.to_vec());
        pos += len;
    }
    if args.is_empty() { None } else { Some(args) }
}
