//! zset algebra `*STORE` forms + `ZINTERCARD`. Argument grammar and
//! error wording mirror the server's `parse_zsetstore_args` /
//! `parse_zintercard_args` in `kevy-rt::exec_build`.

use crate::KevyResult;
use crate::store::Store;

use kevy_store::ZAggregate;

use super::util::{ERR_SYNTAX, arg_u64, emit_int, err, verb_name, wrong_args};

const ERR_NUMKEYS: &str = "ERR numkeys should be greater than 0";
const ERR_KEYS_GT_ARGS: &str = "ERR Number of keys can't be greater than number of args";

/// One zset-algebra request; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"ZINTERSTORE" => cmd_zstore(s, argv, out, false, Store::zinterstore),
        b"ZUNIONSTORE" => cmd_zstore(s, argv, out, false, Store::zunionstore),
        b"ZDIFFSTORE" => {
            cmd_zstore(s, argv, out, true, |s, dst, keys, _w, _a| s.zdiffstore(dst, keys))
        }
        b"ZINTERCARD" => cmd_zintercard(s, argv, out),
        _ => return false,
    }
    true
}

type ZStoreOp = fn(&Store, &[u8], &[&[u8]], Option<&[f64]>, ZAggregate) -> KevyResult<usize>;

/// `VERB dst numkeys key… [WEIGHTS w…] [AGGREGATE SUM|MIN|MAX]`
/// (`diff_form` = ZDIFFSTORE: no WEIGHTS/AGGREGATE allowed).
fn cmd_zstore(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>, diff_form: bool, op: ZStoreOp) {
    if argv.len() < 4 {
        return wrong_args(out, &verb_name(argv));
    }
    let Some(numkeys) = arg_u64(&argv[2]).map(|n| n as usize).filter(|&n| n > 0) else {
        return err(out, ERR_NUMKEYS);
    };
    if argv.len() < 3 + numkeys {
        return err(out, ERR_KEYS_GT_ARGS);
    }
    let keys: Vec<&[u8]> = argv[3..3 + numkeys].iter().map(Vec::as_slice).collect();
    let (weights, aggregate) = match parse_tail(argv, diff_form, numkeys) {
        Ok(t) => t,
        Err(msg) => return err(out, msg),
    };
    emit_int(out, op(s, &argv[1], &keys, weights.as_deref(), aggregate).map(|n| n as i64));
}

/// The optional `[WEIGHTS w…] [AGGREGATE SUM|MIN|MAX]` tail.
fn parse_tail(
    argv: &[Vec<u8>],
    diff_form: bool,
    numkeys: usize,
) -> Result<(Option<Vec<f64>>, ZAggregate), &'static str> {
    let mut weights = None;
    let mut aggregate = ZAggregate::Sum;
    let mut i = 3 + numkeys;
    while i < argv.len() {
        let a = &argv[i];
        if !diff_form && a.eq_ignore_ascii_case(b"WEIGHTS") {
            if argv.len() < i + 1 + numkeys {
                return Err(ERR_SYNTAX);
            }
            let mut w = Vec::with_capacity(numkeys);
            for j in 0..numkeys {
                let v = std::str::from_utf8(&argv[i + 1 + j])
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or("ERR weight value is not a float")?;
                w.push(v);
            }
            weights = Some(w);
            i += 1 + numkeys;
        } else if !diff_form && a.eq_ignore_ascii_case(b"AGGREGATE") {
            let m = argv.get(i + 1).ok_or(ERR_SYNTAX)?;
            aggregate = if m.eq_ignore_ascii_case(b"SUM") {
                ZAggregate::Sum
            } else if m.eq_ignore_ascii_case(b"MIN") {
                ZAggregate::Min
            } else if m.eq_ignore_ascii_case(b"MAX") {
                ZAggregate::Max
            } else {
                return Err(ERR_SYNTAX);
            };
            i += 2;
        } else {
            return Err(ERR_SYNTAX);
        }
    }
    Ok((weights, aggregate))
}

/// `ZINTERCARD numkeys key… [LIMIT n]` — `limit = 0` means unlimited.
fn cmd_zintercard(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 3 {
        return wrong_args(out, &verb_name(argv));
    }
    let Some(numkeys) = arg_u64(&argv[1]).map(|n| n as usize).filter(|&n| n > 0) else {
        return err(out, ERR_NUMKEYS);
    };
    if argv.len() < 2 + numkeys {
        return err(out, ERR_KEYS_GT_ARGS);
    }
    let keys: Vec<&[u8]> = argv[2..2 + numkeys].iter().map(Vec::as_slice).collect();
    let mut limit = 0usize;
    let mut i = 2 + numkeys;
    while i < argv.len() {
        if argv[i].eq_ignore_ascii_case(b"LIMIT") {
            let Some(n) = argv.get(i + 1).and_then(|v| arg_u64(v)) else {
                return err(out, "ERR LIMIT can't be negative");
            };
            limit = n as usize;
            i += 2;
        } else {
            return err(out, ERR_SYNTAX);
        }
    }
    emit_int(out, s.zintercard(&keys, limit).map(|n| n as i64));
}
