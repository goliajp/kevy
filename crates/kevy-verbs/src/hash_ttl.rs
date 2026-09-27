//! Hash field TTLs (Redis 7.4): `HEXPIRE` / `HPEXPIRE` / `HPEXPIREAT` /
//! `HTTL` / `HPTTL` / `HPERSIST`, all shaped
//! `key <arg> [NX|XX|GT|LT] FIELDS <numfields> field [field ...]`
//! (the three reads and `HPERSIST` take no deadline and no condition).
//! Replies are per-field integer arrays in request order.

use kevy_resp::{ArgvView, CmdError, encode_array_len, encode_error, encode_integer};
use kevy_store::{HExpireCond, Store, now_unix_ms};

use crate::args::arg_i64;
use crate::reply::{ERR_NOT_INT, store_err, wrong_args};
use crate::{Effect, changed};

/// One field-TTL command; `None` = the verb is not in this group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    Some(match cmd {
        b"HEXPIRE" => hexpire(store, args, out, "hexpire", |s| {
            now_unix_ms().saturating_add_signed(s.saturating_mul(1000))
        }),
        b"HPEXPIRE" => {
            hexpire(store, args, out, "hpexpire", |ms| now_unix_ms().saturating_add_signed(ms))
        }
        b"HPEXPIREAT" => hexpire(store, args, out, "hpexpireat", |abs| abs.max(0) as u64),
        b"HTTL" => httl(store, args, true, "httl", out),
        b"HPTTL" => httl(store, args, false, "hpttl", out),
        b"HPERSIST" => hpersist(store, args, out),
        _ => return None,
    })
}

/// Parse `[NX|XX|GT|LT] FIELDS n f1..fn` starting at `i`: the condition
/// and the indices of the fields.
fn parse_cond_fields<A: ArgvView + ?Sized>(
    args: &A,
    mut i: usize,
) -> Result<(HExpireCond, Vec<usize>), CmdError> {
    let mut cond = HExpireCond::Always;
    if i < args.len() {
        let a = &args[i];
        let parsed = if a.eq_ignore_ascii_case(b"NX") {
            Some(HExpireCond::Nx)
        } else if a.eq_ignore_ascii_case(b"XX") {
            Some(HExpireCond::Xx)
        } else if a.eq_ignore_ascii_case(b"GT") {
            Some(HExpireCond::Gt)
        } else if a.eq_ignore_ascii_case(b"LT") {
            Some(HExpireCond::Lt)
        } else {
            None
        };
        if let Some(c) = parsed {
            cond = c;
            i += 1;
        }
    }
    if i >= args.len() || !args[i].eq_ignore_ascii_case(b"FIELDS") {
        return Err(CmdError::Wire(
            "ERR Mandatory keyword FIELDS is missing or not at the right position",
        ));
    }
    i += 1;
    let n: usize = args
        .get(i)
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .ok_or("ERR Parameter `numFields` should be greater than 0")?;
    i += 1;
    if args.len() != i + n {
        return Err(CmdError::Wire("ERR Parameter `numFields` is more than number of arguments"));
    }
    Ok((cond, (i..i + n).collect()))
}

fn emit_codes(out: &mut Vec<u8>, codes: &[i8]) {
    encode_array_len(out, codes.len() as i64);
    for c in codes {
        encode_integer(out, i64::from(*c));
    }
}

/// The three deadline-setting forms; `to_abs_ms` turns the raw argument
/// into an absolute unix-ms deadline. A code of 1 (set) or 2 (deleted
/// by a past deadline) is a change.
fn hexpire<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    name: &'static str,
    to_abs_ms: impl Fn(i64) -> u64,
) -> Effect {
    if args.len() < 6 {
        wrong_args(out, name);
        return Effect::Unchanged;
    }
    let Some(raw) = arg_i64(&args[2]) else {
        encode_error(out, ERR_NOT_INT);
        return Effect::Unchanged;
    };
    let (cond, idx) = match parse_cond_fields(args, 3) {
        Ok(t) => t,
        Err(e) => {
            encode_error(out, e.as_wire());
            return Effect::Unchanged;
        }
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| &args[i] as &[u8]).collect();
    match store.hexpire_at(&args[1], &fields, to_abs_ms(raw), cond) {
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

/// `HTTL key FIELDS n f…` (seconds) / `HPTTL …` (milliseconds): `-2`
/// for a missing field, `-1` for one without a TTL. Seconds round to
/// the nearest, as the key-level TTL and Redis both do.
fn httl<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    in_secs: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 5 {
        wrong_args(out, cmd);
        return Effect::Read;
    }
    let (_, idx) = match parse_cond_fields(args, 2) {
        Ok(t) => t,
        Err(e) => {
            encode_error(out, e.as_wire());
            return Effect::Read;
        }
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| &args[i] as &[u8]).collect();
    match store.hpttl(&args[1], &fields) {
        Err(e) => store_err(out, e),
        Ok(ttls) => {
            encode_array_len(out, ttls.len() as i64);
            for ms in ttls {
                encode_integer(out, if in_secs && ms >= 0 { (ms + 500) / 1000 } else { ms });
            }
        }
    }
    Effect::Read
}

/// `HPERSIST key FIELDS n f…`; a code of 1 means a TTL was cleared.
fn hpersist<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() < 5 {
        wrong_args(out, "hpersist");
        return Effect::Unchanged;
    }
    let (_, idx) = match parse_cond_fields(args, 2) {
        Ok(t) => t,
        Err(e) => {
            encode_error(out, e.as_wire());
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
