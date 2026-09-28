//! Multi-key and pub/sub verbs that reach the local dispatcher only
//! when malformed.
//!
//! The runtime serves these through its cross-shard gather; its router
//! falls back to the local dispatcher when a call is too short to route,
//! so the arity refusal is the whole job here. `MSET` and `RENAME`, which
//! also arrive here well-formed when the server replays its own log, are
//! executed by `kevy_verbs::exec` before this table is reached.

use crate::cmd::wrong_args;

/// The arity refusal for a malformed multi-key or pub/sub call.
pub(crate) fn dispatch_multikey_stub(cmd: &[u8], out: &mut Vec<u8>) -> bool {
    let name = match cmd {
        b"MGET" => "mget",
        b"SINTER" => "sinter",
        b"SUNION" => "sunion",
        b"SDIFF" => "sdiff",
        b"KEYS" => "keys",
        b"SCAN" => "scan",
        b"RANDOMKEY" => "randomkey",
        b"SUBSCRIBE" => "subscribe",
        b"PUBLISH" => "publish",
        _ => return false,
    };
    wrong_args(out, name);
    true
}
