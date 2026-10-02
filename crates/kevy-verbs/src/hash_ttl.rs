//! Hash field TTLs (Redis 7.4). The setters — `HEXPIRE` / `HPEXPIRE` /
//! `HEXPIREAT` / `HPEXPIREAT` — are `key <time> …` where a condition
//! (`NX|XX|GT|LT`) and `FIELDS <numfields> field…` may come in either
//! order; the readers — `HTTL` / `HPTTL` / `HEXPIRETIME` / `HPEXPIRETIME`
//! and `HPERSIST` — are `key FIELDS <numfields> field…` and nothing else.
//! Replies are per-field integer arrays in request order.

use kevy_resp::{ArgvView, encode_array_len, encode_error, encode_integer};
use kevy_store::{HExpireCond, Store, now_unix_ms};

use crate::args::arg_i64;
use crate::reply::{ERR_NOT_INT, store_err, wrong_args};
use crate::{Effect, changed};

/// The latest field deadline Redis stores, in unix ms; past it a setter
/// refuses the time.
const MAX_DEADLINE_MS: u64 = (1 << 46) - 1;

/// How a setter's time argument reads.
#[derive(Clone, Copy)]
struct Time {
    secs: bool,
    relative: bool,
}

/// One field-TTL command; `None` = the verb is not in this group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    let (s, ms) = (true, false);
    Some(match cmd {
        b"HEXPIRE" => hexpire(store, args, out, "hexpire", Time { secs: s, relative: true }),
        b"HPEXPIRE" => hexpire(store, args, out, "hpexpire", Time { secs: ms, relative: true }),
        b"HEXPIREAT" => hexpire(store, args, out, "hexpireat", Time { secs: s, relative: false }),
        b"HPEXPIREAT" => {
            hexpire(store, args, out, "hpexpireat", Time { secs: ms, relative: false })
        }
        b"HTTL" => read(store, args, out, "httl", |st, k, f| {
            Ok(st
                .hpttl(k, f)?
                .into_iter()
                .map(|t| if t >= 0 { (t + 500) / 1000 } else { t })
                .collect())
        }),
        b"HPTTL" => read(store, args, out, "hpttl", Store::hpttl),
        b"HEXPIRETIME" => read(store, args, out, "hexpiretime", |st, k, f| {
            Ok(st
                .hexpire_time(k, f)?
                .into_iter()
                .map(|t| if t >= 0 { (t + 999) / 1000 } else { t })
                .collect())
        }),
        b"HPEXPIRETIME" => read(store, args, out, "hpexpiretime", Store::hexpire_time),
        b"HPERSIST" => hpersist(store, args, out),
        _ => return None,
    })
}

/// The absolute unix-ms deadline `raw` names and the "now" it is judged
/// against — one clock read for both, as Redis judges a command at one
/// instant — or the refusal.
fn deadline(raw: i64, t: Time, name: &str) -> Result<(u64, u64), String> {
    if raw < 0 {
        return Err("ERR invalid expire time, must be >= 0".to_string());
    }
    let now = now_unix_ms();
    let ms = if t.secs { raw.checked_mul(1000) } else { Some(raw) };
    let at =
        ms.and_then(|ms| if t.relative { now.checked_add(ms as u64) } else { Some(ms as u64) });
    match at {
        Some(at) if at <= MAX_DEADLINE_MS => Ok((at, now)),
        _ => Err(format!("ERR invalid expire time in '{name}' command")),
    }
}

/// A setter's `[NX|XX|GT|LT]` and `FIELDS n f…`, in either order, from
/// argument 3: the condition and the indices of the fields.
fn setter_tail<A: ArgvView + ?Sized>(args: &A) -> Result<(HExpireCond, Vec<usize>), String> {
    let (mut cond, mut fields) = (None, None);
    let mut i = 3;
    while i < args.len() {
        let a = &args[i];
        let c = [(b"NX", HExpireCond::Nx), (b"XX", HExpireCond::Xx), (b"GT", HExpireCond::Gt)]
            .into_iter()
            .chain([(b"LT", HExpireCond::Lt)])
            .find(|(w, _)| a.eq_ignore_ascii_case(*w));
        if let Some((_, c)) = c {
            if cond.replace(c).is_some() {
                return Err("ERR Multiple condition flags specified".to_string());
            }
            i += 1;
        } else if a.eq_ignore_ascii_case(b"FIELDS") {
            if fields.is_some() {
                return Err("ERR FIELDS keyword specified multiple times".to_string());
            }
            let n = numfields(args, i + 1)
                .ok_or("ERR Parameter `numFields` should be greater than 0")?;
            if i + 2 + n > args.len() {
                return Err("ERR wrong number of arguments".to_string());
            }
            fields = Some((i + 2..i + 2 + n).collect::<Vec<_>>());
            i += 2 + n;
        } else {
            return Err(format!("ERR unknown argument: {}", String::from_utf8_lossy(a)));
        }
    }
    let fields =
        fields.ok_or("ERR Mandatory argument FIELDS is missing or not at the right position")?;
    Ok((cond.unwrap_or(HExpireCond::Always), fields))
}

/// A reader's `FIELDS n f…`, exactly, from argument 2: the field indices.
fn reader_tail<A: ArgvView + ?Sized>(args: &A) -> Result<Vec<usize>, &'static str> {
    if !args[2].eq_ignore_ascii_case(b"FIELDS") {
        return Err("ERR Mandatory argument FIELDS is missing or not at the right position");
    }
    let n = numfields(args, 3).ok_or("ERR Number of fields must be a positive integer")?;
    if args.len() != 4 + n {
        return Err("ERR The `numfields` parameter must match the number of arguments");
    }
    Ok((4..4 + n).collect())
}

fn numfields<A: ArgvView + ?Sized>(args: &A, at: usize) -> Option<usize> {
    args.get(at).and_then(arg_i64).filter(|&n| n > 0).map(|n| n as usize)
}

fn emit_codes<T: Copy + Into<i64>>(out: &mut Vec<u8>, codes: &[T]) {
    encode_array_len(out, codes.len() as i64);
    for c in codes {
        encode_integer(out, (*c).into());
    }
}

/// A setter. A code of 1 (set) or 2 (deleted by a past deadline) is a
/// change.
fn hexpire<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    name: &'static str,
    t: Time,
) -> Effect {
    if args.len() < 6 {
        wrong_args(out, name);
        return Effect::Unchanged;
    }
    let Some(raw) = arg_i64(&args[2]) else {
        encode_error(out, ERR_NOT_INT);
        return Effect::Unchanged;
    };
    let parsed = deadline(raw, t, name).and_then(|at| setter_tail(args).map(|tail| (at, tail)));
    let ((at, now), (cond, idx)) = match parsed {
        Ok(p) => p,
        Err(e) => {
            encode_error(out, &e);
            return Effect::Unchanged;
        }
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| &args[i] as &[u8]).collect();
    match store.hexpire_as_of(&args[1], &fields, at, now, cond) {
        Err(e) => {
            store_err(out, e);
            Effect::Unchanged
        }
        Ok(codes) => {
            emit_codes(out, &codes);
            changed(codes.iter().any(|&c| c == 1 || c == 2))
        }
    }
}

type FieldRead = fn(&mut Store, &[u8], &[&[u8]]) -> Result<Vec<i64>, kevy_store::StoreError>;

/// A reader: `-2` for a missing field, `-1` for one without a TTL.
fn read<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    name: &str,
    f: FieldRead,
) -> Effect {
    if args.len() < 5 {
        wrong_args(out, name);
        return Effect::Read;
    }
    let idx = match reader_tail(args) {
        Ok(idx) => idx,
        Err(e) => {
            encode_error(out, e);
            return Effect::Read;
        }
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| &args[i] as &[u8]).collect();
    match f(store, &args[1], &fields) {
        Err(e) => store_err(out, e),
        Ok(v) => emit_codes(out, &v),
    }
    Effect::Read
}

/// `HPERSIST key FIELDS n f…`; a code of 1 means a TTL was cleared.
fn hpersist<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() < 5 {
        wrong_args(out, "hpersist");
        return Effect::Unchanged;
    }
    let idx = match reader_tail(args) {
        Ok(idx) => idx,
        Err(e) => {
            encode_error(out, e);
            return Effect::Unchanged;
        }
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| &args[i] as &[u8]).collect();
    match store.hpersist(&args[1], &fields) {
        Err(e) => {
            store_err(out, e);
            Effect::Unchanged
        }
        Ok(codes) => {
            emit_codes(out, &codes);
            changed(codes.contains(&1))
        }
    }
}
