//! The verbs the engine writes into its own AOF but the routed client
//! path never executes locally.
//!
//! `MSET` and `RENAME` are served to clients by the runtime's
//! cross-shard machinery (`Route::MSet` / `Route::Rename` → an `Op` on
//! the owning shard), and the op records its effect into the AOF using
//! the same verb. But **replay and replica-apply both go through the
//! local dispatcher** (`shard_run::replay_dispatch` →
//! `Commands::dispatch`; `replication_apply::apply_replica_frame` →
//! `dispatch_into`), where these verbs had no implementation at all —
//! `MSET` answered with an arity error and `RENAME` was unknown.
//!
//! Measured consequence, `appendfsync always`, single restart:
//! `MSET m:1 a m:2 b` acknowledged `+OK`, read back fine, and every key
//! was **gone** after the restart; `RENAME src dst` came back *reverted*
//! — `src` alive again, `dst` missing. A replica never saw either.
//!
//! So these are not stubs to satisfy a match arm: they are the replay
//! half of two shipped verbs. A malformed call still answers the arity
//! error, which is what the client path relied on.

use kevy_resp::{ArgvView, encode_error, encode_integer, encode_simple_string};
use kevy_store::{RenameOutcome, Store};

use crate::cmd::wrong_args;

/// `MSET k v [k v …]` — applied, not refused. Well-formed means an odd
/// argc of at least 3 (verb + n pairs), the same shape the router
/// accepts before it splits the pairs per shard.
pub(crate) fn replay_mset<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return wrong_args(out, "mset");
    }
    let mut i = 1;
    while i + 1 < args.len() {
        store.set(&args[i], args[i + 1].to_vec(), None, false, false);
        i += 2;
    }
    encode_simple_string(out, "OK");
}

/// `RENAME src dst` / `RENAMENX src dst` — the same
/// [`Store::rename`] the same-shard op runs, so a replayed record
/// reproduces the op exactly (including TTL carry-over and the NX
/// refusal).
pub(crate) fn replay_rename<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    nx: bool,
    out: &mut Vec<u8>,
) {
    if args.len() != 3 {
        return wrong_args(out, if nx { "renamenx" } else { "rename" });
    }
    match store.rename(&args[1], &args[2], nx) {
        RenameOutcome::Renamed if nx => encode_integer(out, 1),
        RenameOutcome::Renamed => encode_simple_string(out, "OK"),
        RenameOutcome::DstExists => encode_integer(out, 0),
        RenameOutcome::NoSuchSrc => encode_error(out, "ERR no such key"),
    }
}

/// Multi-key & pub/sub verbs are served by the runtime's cross-shard
/// gather. Most of them only reach `dispatch` when malformed (route fell
/// back to `Local`), so the arity error is the whole job — but the two
/// the engine writes into its own AOF (`MSET`, `RENAME`) also arrive
/// here well-formed on replay and on replica apply, and refusing them
/// there loses the write. Those two execute, above.
pub(crate) fn dispatch_multikey_stub<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"MSET" => replay_mset(store, args, out),
        b"RENAME" => replay_rename(store, args, false, out),
        b"RENAMENX" => replay_rename(store, args, true, out),
        b"MGET" => wrong_args(out, "mget"),
        b"SINTER" => wrong_args(out, "sinter"),
        b"SUNION" => wrong_args(out, "sunion"),
        b"SDIFF" => wrong_args(out, "sdiff"),
        b"KEYS" => wrong_args(out, "keys"),
        b"SCAN" => wrong_args(out, "scan"),
        b"RANDOMKEY" => wrong_args(out, "randomkey"),
        b"SUBSCRIBE" => wrong_args(out, "subscribe"),
        b"PUBLISH" => wrong_args(out, "publish"),
        _ => return false,
    }
    true
}
