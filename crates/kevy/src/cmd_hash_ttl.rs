//! v2.4 — hash field-TTL commands (Redis 7.4):
//! `HEXPIRE` / `HPEXPIRE` / `HPEXPIREAT` / `HTTL` / `HPERSIST`, all
//! `key <arg> [NX|XX|GT|LT] FIELDS <numfields> field [field ...]`
//! shaped (HTTL/HPERSIST take no condition/ttl). Replies are per-field
//! integer arrays in request order.
//!
//! AOF: the relative forms get an absolute `HPEXPIREAT` follow-up
//! frame (same INC-2026-06-09 discipline as `EXPIRE` → `PEXPIREAT`) —
//! see `Shard::log_write`'s hash extension in kevy-rt.

use kevy_resp::CmdError;
use kevy_resp::{ArgvView, encode_array_len, encode_error, encode_integer};
use kevy_store::{HExpireCond, Store, now_unix_ms};

use crate::cmd::{ERR_NOT_INT, arg_i64, store_err, wrong_args};

/// Parse `[NX|XX|GT|LT] FIELDS n f1..fn` starting at `i`. Returns the
/// condition + the field slice indices.
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
        return Err(CmdError::Wire("ERR Mandatory keyword FIELDS is missing or not at the right position"));
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

/// Shared body for the three deadline-setting forms; `to_abs_ms`
/// converts the raw arg to an absolute unix-ms deadline.
fn hexpire_generic<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    name: &'static str,
    to_abs_ms: impl Fn(i64) -> u64,
) {
    if args.len() < 6 {
        return wrong_args(out, name);
    }
    let Some(raw) = arg_i64(&args[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    let (cond, idx) = match parse_cond_fields(args, 3) {
        Ok(t) => t,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| &args[i] as &[u8]).collect();
    let deadline = to_abs_ms(raw);
    match store.hexpire_at(&args[1], &fields, deadline, cond) {
        Err(e) => store_err(out, e),
        Ok(codes) => emit_codes(out, &codes),
    }
}

/// `HEXPIRE key seconds [cond] FIELDS n f…`.
pub(crate) fn cmd_hexpire<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    hexpire_generic(store, args, out, "hexpire", |s| {
        now_unix_ms().saturating_add_signed(s.saturating_mul(1000))
    });
}

/// `HPEXPIRE key ms [cond] FIELDS n f…`.
pub(crate) fn cmd_hpexpire<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    hexpire_generic(store, args, out, "hpexpire", |ms| {
        now_unix_ms().saturating_add_signed(ms)
    });
}

/// `HPEXPIREAT key unix-ms [cond] FIELDS n f…` — the canonical
/// (absolute) form the AOF carries.
pub(crate) fn cmd_hpexpireat<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    hexpire_generic(store, args, out, "hpexpireat", |abs| abs.max(0) as u64);
}

/// `HTTL key FIELDS n f…` — remaining ms per field (`-2` missing,
/// `-1` no TTL).
pub(crate) fn cmd_httl<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 5 {
        return wrong_args(out, "httl");
    }
    let (_, idx) = match parse_cond_fields(args, 2) {
        Ok(t) => t,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| &args[i] as &[u8]).collect();
    match store.httl(&args[1], &fields) {
        Err(e) => store_err(out, e),
        Ok(ttls) => {
            encode_array_len(out, ttls.len() as i64);
            for t in ttls {
                encode_integer(out, t);
            }
        }
    }
}

/// `HPERSIST key FIELDS n f…`.
pub(crate) fn cmd_hpersist<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 5 {
        return wrong_args(out, "hpersist");
    }
    let (_, idx) = match parse_cond_fields(args, 2) {
        Ok(t) => t,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| &args[i] as &[u8]).collect();
    match store.hpersist(&args[1], &fields) {
        Err(e) => store_err(out, e),
        Ok(codes) => emit_codes(out, &codes),
    }
}
