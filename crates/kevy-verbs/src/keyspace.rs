//! Type-agnostic key commands, as they apply to one store: every key
//! named is taken to live in it. Spreading a multi-key call across
//! shards is the caller's job.

use std::time::Duration;

use kevy_resp::{ArgvView, encode_error, encode_integer, encode_simple_string};
use kevy_store::{RenameOutcome, Store};

use crate::args::{arg_i64, rest_borrowed, upper_verb};
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
        b"EXPIRE" => expire(store, args, 1000, false, "expire", out),
        b"PEXPIRE" => expire(store, args, 1, false, "pexpire", out),
        b"EXPIREAT" => expire(store, args, 1000, true, "expireat", out),
        b"PEXPIREAT" => expire(store, args, 1, true, "pexpireat", out),
        b"TTL" => ttl(store, args, true, "ttl", out),
        b"PTTL" => ttl(store, args, false, "pttl", out),
        b"EXPIRETIME" => expire_time(store, args, true, "expiretime", out),
        b"PEXPIRETIME" => expire_time(store, args, false, "pexpiretime", out),
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

/// `EXPIRE` / `PEXPIRE` (from now) and `EXPIREAT` / `PEXPIREAT` (a unix
/// time) `key time [NX | XX | GT | LT]`. A deadline already past deletes
/// the key, answering 1 if it existed, as Redis does.
fn expire<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    unit_ms: i64,
    absolute: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 3 {
        wrong_args(out, cmd);
        return Effect::Unchanged;
    }
    let cond = match ExpireCond::parse(args) {
        Ok(c) => c,
        Err(e) => {
            encode_error(out, &e);
            return Effect::Unchanged;
        }
    };
    let Some(ms) = expire_ms(&args[2], unit_ms, absolute, cmd, out) else {
        return Effect::Unchanged;
    };
    let key = &args[1];
    // the probe that writes also decides existence: a separate check reads
    // the cached clock while the write reads a fresh one, so a key lapsed
    // between the two would be answered as present yet written as absent
    let set = match (cond, absolute) {
        (None, false) if ms <= 0 => store.del(&[key]) == 1,
        (None, false) => store.expire(key, Duration::from_millis(ms as u64)),
        (None, true) => store.expire_at_unix_ms(key, ms.max(0) as u64),
        (Some(c), _) => expire_if(store, key, c, if absolute { ms } else { ms + now_ms() }),
    };
    encode_integer(out, i64::from(set));
    changed(set)
}

fn now_ms() -> i64 {
    kevy_store::now_unix_ms() as i64
}

/// The time argument in milliseconds, or `None` once the error is written:
/// not an integer, or out of what Redis accepts — seconds whose
/// milliseconds overflow, or a relative time whose deadline would.
fn expire_ms(
    raw: &[u8],
    unit_ms: i64,
    absolute: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) -> Option<i64> {
    let Some(n) = arg_i64(raw) else {
        encode_error(out, ERR_NOT_INT);
        return None;
    };
    let ms = n.checked_mul(unit_ms);
    // the clock is read only when the sum could overflow: a unix time in
    // milliseconds stays below 2^42 for the next century
    let fits =
        |ms: i64| absolute || ms <= i64::MAX - (1 << 42) || ms.checked_add(now_ms()).is_some();
    match ms {
        Some(ms) if fits(ms) => Some(ms),
        _ => {
            encode_error(out, &format!("ERR invalid expire time in '{cmd}' command"));
            None
        }
    }
}

/// Apply the deadline `at` (unix ms) to `key` when `c` allows it.
fn expire_if(store: &mut Store, key: &[u8], c: ExpireCond, at: i64) -> bool {
    if !store.key_exists(key) || !c.allows(at, store.deadline_unix_ms(key)) {
        return false;
    }
    if at <= now_ms() {
        return store.del(&[key]) == 1;
    }
    store.expire_at_unix_ms(key, at as u64)
}

/// The `NX | XX | GT | LT` conditions of the `EXPIRE` family.
#[derive(Clone, Copy, Default)]
struct ExpireCond {
    nx: bool,
    xx: bool,
    gt: bool,
    lt: bool,
}

impl ExpireCond {
    /// The conditions after the time, `None` when there are none; the
    /// errors are Redis's, checked before the time is read.
    fn parse<A: ArgvView + ?Sized>(args: &A) -> Result<Option<Self>, String> {
        let mut c = Self::default();
        let mut buf = [0u8; 32];
        for i in 3..args.len() {
            match upper_verb(&args[i], &mut buf) {
                b"NX" => c.nx = true,
                b"XX" => c.xx = true,
                b"GT" => c.gt = true,
                b"LT" => c.lt = true,
                _ => {
                    let opt = String::from_utf8_lossy(&args[i]);
                    return Err(format!("ERR Unsupported option {opt}"));
                }
            }
        }
        if c.nx && (c.xx || c.gt || c.lt) {
            return Err(
                "ERR NX and XX, GT or LT options at the same time are not compatible".into()
            );
        }
        if c.gt && c.lt {
            return Err("ERR GT and LT options at the same time are not compatible".into());
        }
        Ok((args.len() > 3).then_some(c))
    }

    /// Whether the new deadline `at` may replace `cur`; no deadline counts
    /// as an infinite one.
    fn allows(self, at: i64, cur: Option<u64>) -> bool {
        let cur = cur.map(|c| c as i64);
        !(self.nx && cur.is_some())
            && !(self.xx && cur.is_none())
            && !(self.gt && cur.is_none_or(|c| at <= c))
            && !(self.lt && cur.is_some_and(|c| at >= c))
    }
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

/// `EXPIRETIME` / `PEXPIRETIME key`: the absolute Unix deadline, `-1`
/// without a TTL, `-2` for a missing key; seconds round to the nearest.
fn expire_time<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    in_secs: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() != 2 {
        wrong_args(out, cmd);
    } else {
        let ms = store.pexpire_time(&args[1]);
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
