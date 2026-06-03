//! Command-body helpers for the type-shaped operations: SET / SETEX /
//! INCR / EXPIRE / LPOP / SPOP / TTL. Split out so [`crate::cmd`] stays
//! under the 500-LOC house rule. Same `pub(crate)` shape as the rest —
//! the dispatch tables in `crate::dispatch` + `crate::dispatch_collections`
//! call these directly.

use crate::cmd::{ERR_NOT_INT, arg_i64, emit_int_result, store_err, wrong_args};
use kevy_resp::{
    Argv, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
    encode_simple_string,
};
use kevy_store::Store;
use std::time::Duration;

/// `SPOP`/`SRANDMEMBER key [count]` — single reply without count, array with it.
pub(crate) fn cmd_spop_rand(store: &mut Store, args: &Argv, remove: bool, out: &mut Vec<u8>) {
    let name = if remove { "spop" } else { "srandmember" };
    if args.len() < 2 || args.len() > 3 {
        return wrong_args(out, name);
    }
    let count_given = args.len() == 3;
    let count = if count_given {
        match arg_i64(&args[2]) {
            Some(c) if c >= 0 => c as usize,
            _ => return encode_error(out, "ERR value is out of range, must be positive"),
        }
    } else {
        1
    };
    let res = if remove {
        store.spop(&args[1], count)
    } else {
        store.srandmember(&args[1], count)
    };
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => {
            if count_given {
                encode_array_len(out, items.len() as i64);
                for it in &items {
                    encode_bulk(out, it);
                }
            } else {
                match items.first() {
                    Some(v) => encode_bulk(out, v),
                    None => encode_null_bulk(out),
                }
            }
        }
    }
}

/// `LPOP`/`RPOP key [count]` — single reply without count, array with it.
pub(crate) fn cmd_pop(store: &mut Store, args: &Argv, tail: bool, out: &mut Vec<u8>) {
    let name = if tail { "rpop" } else { "lpop" };
    if args.len() < 2 || args.len() > 3 {
        return wrong_args(out, name);
    }
    let count_given = args.len() == 3;
    let count = if count_given {
        match arg_i64(&args[2]) {
            Some(c) if c >= 0 => c as usize,
            _ => return encode_error(out, "ERR value is out of range, must be positive"),
        }
    } else {
        1
    };
    let res = if tail {
        store.rpop(&args[1], count)
    } else {
        store.lpop(&args[1], count)
    };
    match res {
        Err(e) => store_err(out, e),
        Ok(items) => {
            if count_given {
                if items.is_empty() {
                    out.extend_from_slice(b"*-1\r\n"); // nil array (key absent / empty)
                } else {
                    encode_array_len(out, items.len() as i64);
                    for it in &items {
                        encode_bulk(out, it);
                    }
                }
            } else {
                match items.first() {
                    Some(v) => encode_bulk(out, v),
                    None => encode_null_bulk(out),
                }
            }
        }
    }
}

/// `SET key value [EX s | PX ms] [NX | XX]`.
pub(crate) fn cmd_set(store: &mut Store, args: &Argv, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "set");
    }
    let mut expire: Option<Duration> = None;
    let mut nx = false;
    let mut xx = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].to_ascii_uppercase().as_slice() {
            b"NX" => nx = true,
            b"XX" => xx = true,
            opt @ (b"EX" | b"PX") => {
                let Some(raw) = args.get(i + 1) else {
                    return encode_error(out, "ERR syntax error");
                };
                let Some(n) = arg_i64(raw).filter(|&n| n > 0) else {
                    return encode_error(out, "ERR invalid expire time in 'set' command");
                };
                let ms = if opt == b"EX" {
                    n.saturating_mul(1000)
                } else {
                    n
                };
                expire = Some(Duration::from_millis(ms as u64));
                i += 1;
            }
            _ => return encode_error(out, "ERR syntax error"),
        }
        i += 1;
    }
    if nx && xx {
        return encode_error(out, "ERR syntax error");
    }
    if store.set(&args[1], args[2].to_vec(), expire, nx, xx) {
        encode_simple_string(out, "OK");
    } else {
        encode_null_bulk(out); // NX/XX condition not met
    }
}

/// `SETEX`/`PSETEX key ttl value`.
pub(crate) fn cmd_setex(
    store: &mut Store,
    args: &Argv,
    unit_ms: i64,
    name: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 4 {
        return wrong_args(out, name);
    }
    let Some(n) = arg_i64(&args[2]).filter(|&n| n > 0) else {
        return encode_error(out, &format!("ERR invalid expire time in '{name}' command"));
    };
    let ms = n.saturating_mul(unit_ms) as u64;
    store.set(
        &args[1],
        args[3].to_vec(),
        Some(Duration::from_millis(ms)),
        false,
        false,
    );
    encode_simple_string(out, "OK");
}

pub(crate) fn cmd_incr(
    store: &mut Store,
    args: &Argv,
    delta: i64,
    cmd: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 2 {
        return wrong_args(out, cmd);
    }
    emit_int_result(store.incr_by(&args[1], delta), out);
}

pub(crate) fn cmd_incr_by(
    store: &mut Store,
    args: &Argv,
    negate: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 3 {
        return wrong_args(out, cmd);
    }
    let Some(mut delta) = arg_i64(&args[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    if negate {
        let Some(neg) = delta.checked_neg() else {
            return encode_error(out, "ERR decrement would overflow");
        };
        delta = neg;
    }
    emit_int_result(store.incr_by(&args[1], delta), out);
}

/// `EXPIRE`/`PEXPIRE`: non-positive TTL deletes the key (returning 1 if it
/// existed), matching Redis.
pub(crate) fn cmd_expire(
    store: &mut Store,
    args: &Argv,
    unit_ms: i64,
    cmd: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 3 {
        return wrong_args(out, cmd);
    }
    let Some(n) = arg_i64(&args[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    if store.exists(&[args[1].to_vec()]) == 0 {
        return encode_integer(out, 0);
    }
    if n <= 0 {
        store.del(&[args[1].to_vec()]);
        return encode_integer(out, 1);
    }
    let ms = n.saturating_mul(unit_ms) as u64;
    encode_integer(
        out,
        store.expire(&args[1], Duration::from_millis(ms)) as i64,
    );
}

/// `TTL` (seconds) / `PTTL` (millis). Pass-through of the -2 / -1 sentinels.
pub(crate) fn cmd_ttl(
    store: &mut Store,
    args: &Argv,
    in_secs: bool,
    cmd: &str,
    out: &mut Vec<u8>,
) {
    if args.len() != 2 {
        return wrong_args(out, cmd);
    }
    let ms = store.pttl(&args[1]);
    let val = if in_secs && ms >= 0 {
        (ms + 500) / 1000
    } else {
        ms
    };
    encode_integer(out, val);
}
