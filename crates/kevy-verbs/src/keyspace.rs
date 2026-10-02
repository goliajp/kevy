//! Type-agnostic key commands, as they apply to one store: every key
//! named is taken to live in it. Spreading a multi-key call across
//! shards is the caller's job.

use std::time::Duration;

use kevy_resp::{ArgvView, encode_error, encode_integer, encode_simple_string};
use kevy_store::{RenameOutcome, Store};

use crate::args::{arg_i64, rest_borrowed};
use crate::reply::{ERR_NOT_INT, wrong_args};
use crate::{Effect, changed};

/// One key command; `None` = the verb is not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per key verb.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    Some(match cmd {
        // UNLINK is Redis's background delete; with one thread per
        // store there is nothing to defer, so it is DEL
        b"DEL" | b"UNLINK" => {
            if args.len() < 2 {
                wrong_args(out, if cmd == b"DEL" { "del" } else { "unlink" });
                return Some(Effect::Unchanged);
            }
            let n = store.del(&rest_borrowed(args, 1));
            encode_integer(out, n as i64);
            changed(n > 0)
        }
        // the existence check is what refreshes a key's eviction
        // bookkeeping, so TOUCH answers exactly as EXISTS does
        b"EXISTS" | b"TOUCH" => {
            if args.len() < 2 {
                wrong_args(out, if cmd == b"EXISTS" { "exists" } else { "touch" });
            } else {
                encode_integer(out, store.exists(&rest_borrowed(args, 1)) as i64);
            }
            Effect::Read
        }
        b"EXPIRE" => expire(store, args, 1000, "expire", out),
        b"PEXPIRE" => expire(store, args, 1, "pexpire", out),
        b"EXPIREAT" => expireat(store, args, 1000, "expireat", out),
        b"PEXPIREAT" => expireat(store, args, 1, "pexpireat", out),
        b"TTL" => ttl(store, args, true, "ttl", out),
        b"PTTL" => ttl(store, args, false, "pttl", out),
        b"PERSIST" => {
            if args.len() != 2 {
                wrong_args(out, "persist");
                return Some(Effect::Unchanged);
            }
            let cleared = store.persist(&args[1]);
            encode_integer(out, i64::from(cleared));
            changed(cleared)
        }
        b"TYPE" => {
            if args.len() == 2 {
                encode_simple_string(out, store.type_of(&args[1]));
            } else {
                wrong_args(out, "type");
            }
            Effect::Read
        }
        b"DBSIZE" => {
            encode_integer(out, store.dbsize() as i64);
            Effect::Read
        }
        b"FLUSHDB" | b"FLUSHALL" => {
            store.flushall();
            encode_simple_string(out, "OK");
            Effect::Write
        }
        b"MSET" => mset(store, args, out),
        b"MSETNX" => msetnx(store, args, out),
        b"RENAME" => rename(store, args, false, out),
        b"RENAMENX" => rename(store, args, true, out),
        _ => return None,
    })
}

/// `EXPIRE` / `PEXPIRE`: a non-positive TTL deletes the key, answering
/// 1 if it existed, as Redis does.
fn expire<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    unit_ms: i64,
    cmd: &str,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() != 3 {
        wrong_args(out, cmd);
        return Effect::Unchanged;
    }
    let Some(n) = arg_i64(&args[2]) else {
        encode_error(out, ERR_NOT_INT);
        return Effect::Unchanged;
    };
    // the probe that writes also decides existence: a separate check reads
    // the cached clock while the write reads a fresh one, so a key lapsed
    // between the two would be answered as present yet written as absent
    let set = if n <= 0 {
        store.del(&[&args[1]]) == 1
    } else {
        store.expire(&args[1], Duration::from_millis(n.saturating_mul(unit_ms) as u64))
    };
    encode_integer(out, i64::from(set));
    changed(set)
}

/// `EXPIREAT` (seconds) / `PEXPIREAT` (milliseconds): an absolute unix
/// deadline, so it replays to the same instant. A past one deletes.
fn expireat<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    unit_ms: i64,
    cmd: &str,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() != 3 {
        wrong_args(out, cmd);
        return Effect::Unchanged;
    }
    let Some(n) = arg_i64(&args[2]) else {
        encode_error(out, ERR_NOT_INT);
        return Effect::Unchanged;
    };
    let deadline_ms = n.saturating_mul(unit_ms).max(0) as u64;
    let set = store.expire_at_unix_ms(&args[1], deadline_ms);
    encode_integer(out, i64::from(set));
    changed(set)
}

/// `TTL` (seconds, rounded to nearest) / `PTTL` (milliseconds); the -2
/// and -1 sentinels pass through.
fn ttl<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    in_secs: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() != 2 {
        wrong_args(out, cmd);
    } else {
        let ms = store.pttl(&args[1]);
        encode_integer(out, if in_secs && ms >= 0 { (ms + 500) / 1000 } else { ms });
    }
    Effect::Read
}

/// `MSET k v [k v …]` with every pair applied here. The server's own
/// log records its per-shard share of an `MSET` this way, so this is
/// also how that record is replayed.
fn mset<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        wrong_args(out, "mset");
        return Effect::Unchanged;
    }
    let mut i = 1;
    while i + 1 < args.len() {
        store.set(&args[i], args[i + 1].to_vec(), None, kevy_store::SetCondition::Always);
        i += 2;
    }
    encode_simple_string(out, "OK");
    Effect::Write
}

/// `MSETNX key value [key value …]`: every pair, or — when any of the
/// keys exists — none.
fn msetnx<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        wrong_args(out, "msetnx");
        return Effect::Unchanged;
    }
    if (1..args.len()).step_by(2).any(|i| store.key_exists(&args[i])) {
        encode_integer(out, 0);
        return Effect::Unchanged;
    }
    for i in (1..args.len()).step_by(2) {
        store.set(&args[i], args[i + 1].to_vec(), None, kevy_store::SetCondition::Always);
    }
    encode_integer(out, 1);
    Effect::Write
}

/// `RENAME` / `RENAMENX src dst` with both keys here, TTL carried over.
fn rename<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    nx: bool,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() != 3 {
        wrong_args(out, if nx { "renamenx" } else { "rename" });
        return Effect::Unchanged;
    }
    let outcome =
        if nx { store.rename_nx(&args[1], &args[2]) } else { store.rename(&args[1], &args[2]) };
    match outcome {
        RenameOutcome::Renamed if nx => encode_integer(out, 1),
        RenameOutcome::Renamed => encode_simple_string(out, "OK"),
        RenameOutcome::DstExists => {
            encode_integer(out, 0);
            return Effect::Unchanged;
        }
        RenameOutcome::NoSuchSrc => {
            encode_error(out, "ERR no such key");
            return Effect::Unchanged;
        }
    }
    Effect::Write
}
