//! Keyspace verbs: DEL / EXISTS / TYPE / TTL family / KEYS / SCAN /
//! RANDOMKEY / RENAME / COPY / TOUCH / TIME / DBSIZE / FLUSHALL.

use std::time::Duration;

use crate::store::Store;

use super::util::{
    arg_i64, arg_u64, arr, bulk, emit_int, err, int, kevy_err, opt_bulk, rest, simple,
    wrong_args, ERR_NOT_INT, ERR_SYNTAX,
};

/// One keyspace request; `false` = verb not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per keyspace verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        // These three used to answer `:0` here, because that is what the
        // server answered: its router sent a keyless call to the multi-key
        // fan-out, which summed zero targets. The comment that stood here
        // said to mirror it. It was mirroring a defect — redis 8.10.1
        // answers all three with the arity sentence, and the two surfaces
        // agreeing on a wrong answer is still agreement, which is why the
        // differential harness could not tell. The server routes a keyless
        // call locally now, and both say what Redis says.
        b"DEL" | b"UNLINK" => {
            if argv.len() < 2 {
                wrong_args(out, if up == b"DEL" { "del" } else { "unlink" });
            } else {
                emit_int(out, s.del(&rest(argv, 1)).map(|n| n as i64));
            }
        }
        b"EXISTS" => {
            if argv.len() < 2 {
                wrong_args(out, "exists");
            } else {
                emit_int(out, s.exists(&rest(argv, 1)).map(|n| n as i64));
            }
        }
        b"TYPE" => {
            if argv.len() == 2 {
                simple(out, s.type_of(&argv[1]));
            } else {
                wrong_args(out, "type");
            }
        }
        b"TTL" => cmd_ttl(s, argv, true, "ttl", out),
        b"PTTL" => cmd_ttl(s, argv, false, "pttl", out),
        b"EXPIRE" => cmd_expire(s, argv, 1000, "expire", out),
        b"PEXPIRE" => cmd_expire(s, argv, 1, "pexpire", out),
        b"EXPIREAT" => cmd_expireat(s, argv, true, "expireat", out),
        b"PEXPIREAT" => cmd_expireat(s, argv, false, "pexpireat", out),
        b"PERSIST" => {
            if argv.len() == 2 {
                match s.persist(&argv[1]) {
                    Ok(touched) => int(out, i64::from(touched)),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                wrong_args(out, "persist");
            }
        }
        b"KEYS" => {
            if argv.len() == 2 {
                let keys = s.keys(Some(&argv[1]), None);
                arr(out, keys.len());
                for k in keys {
                    bulk(out, &k);
                }
            } else {
                wrong_args(out, "keys");
            }
        }
        b"SCAN" => cmd_scan(s, argv, out),
        b"RANDOMKEY" => {
            if argv.len() == 1 {
                opt_bulk(out, s.randomkey());
            } else {
                wrong_args(out, "randomkey");
            }
        }
        b"RENAME" => cmd_rename(s, argv, out, false),
        b"RENAMENX" => cmd_rename(s, argv, out, true),
        b"COPY" => cmd_copy(s, argv, out),
        b"TOUCH" => {
            if argv.len() < 2 {
                wrong_args(out, "touch");
            } else {
                emit_int(out, s.touch(&rest(argv, 1)).map(|n| n as i64));
            }
        }
        b"TIME" => {
            let (secs, micros) = s.time();
            arr(out, 2);
            bulk(out, secs.to_string().as_bytes());
            bulk(out, micros.to_string().as_bytes());
        }
        // The server answers DBSIZE / FLUSHALL regardless of extra
        // args — mirror that tolerance.
        b"DBSIZE" => int(out, s.dbsize() as i64),
        b"FLUSHALL" => match s.flushall() {
            Ok(()) => simple(out, "OK"),
            Err(e) => kevy_err(out, &e),
        },
        _ => return false,
    }
    true
}

/// `TTL` (seconds, server rounding) / `PTTL` (millis) — the -2 / -1
/// sentinels pass through untouched.
fn cmd_ttl(s: &Store, argv: &[Vec<u8>], in_secs: bool, name: &str, out: &mut Vec<u8>) {
    if argv.len() != 2 {
        return wrong_args(out, name);
    }
    let ms = s.ttl_ms(&argv[1]);
    int(out, if in_secs && ms >= 0 { (ms + 500) / 1000 } else { ms });
}

/// `EXPIRE`/`PEXPIRE`: a non-positive TTL deletes the key (returning 1
/// if it existed), matching the server.
fn cmd_expire(s: &Store, argv: &[Vec<u8>], unit_ms: i64, name: &str, out: &mut Vec<u8>) {
    if argv.len() != 3 {
        return wrong_args(out, name);
    }
    let Some(n) = arg_i64(&argv[2]) else {
        return err(out, ERR_NOT_INT);
    };
    let res = (|| {
        if s.exists(&[argv[1].as_slice()])? == 0 {
            return Ok(0);
        }
        if n <= 0 {
            s.del(&[argv[1].as_slice()])?;
            return Ok(1);
        }
        let ms = n.saturating_mul(unit_ms) as u64;
        Ok(i64::from(s.expire(&argv[1], Duration::from_millis(ms))?))
    })();
    emit_int(out, res);
}

/// `EXPIREAT` (seconds) / `PEXPIREAT` (millis) — absolute deadlines;
/// a past timestamp expires the key immediately.
fn cmd_expireat(s: &Store, argv: &[Vec<u8>], in_secs: bool, name: &str, out: &mut Vec<u8>) {
    if argv.len() != 3 {
        return wrong_args(out, name);
    }
    let Some(n) = arg_i64(&argv[2]) else {
        return err(out, ERR_NOT_INT);
    };
    let res = (|| {
        if s.exists(&[argv[1].as_slice()])? == 0 {
            return Ok(0);
        }
        let at = n.max(0) as u64;
        let ok = if in_secs { s.expireat(&argv[1], at)? } else { s.pexpireat(&argv[1], at)? };
        Ok(i64::from(ok))
    })();
    emit_int(out, res);
}

/// `SCAN cursor [MATCH pattern] [COUNT n] [TYPE type]` — the embedded
/// cursor is a snapshot offset (single stream), not the server's
/// shard-encoded cursor; the `[cursor, keys]` envelope is identical.
fn cmd_scan(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 2 {
        return wrong_args(out, "scan");
    }
    let Some(cursor) = arg_u64(&argv[1]) else {
        return err(out, "ERR invalid cursor");
    };
    let mut pattern: Option<&[u8]> = None;
    let mut count = 10usize; // Redis default work bound
    let mut type_filter: Option<&[u8]> = None;
    let mut i = 2;
    while i < argv.len() {
        let Some(val) = argv.get(i + 1) else {
            return err(out, ERR_SYNTAX);
        };
        if argv[i].eq_ignore_ascii_case(b"MATCH") {
            pattern = Some(val.as_slice());
        } else if argv[i].eq_ignore_ascii_case(b"COUNT") {
            let Some(n) = arg_i64(val) else {
                return err(out, ERR_NOT_INT);
            };
            if n < 1 {
                return err(out, ERR_SYNTAX);
            }
            count = n as usize;
        } else if argv[i].eq_ignore_ascii_case(b"TYPE") {
            type_filter = Some(val.as_slice());
        } else {
            return err(out, ERR_SYNTAX);
        }
        i += 2;
    }
    let (next, mut keys) = s.scan(cursor, pattern, count);
    if let Some(t) = type_filter {
        keys.retain(|k| s.type_of(k).as_bytes() == t);
    }
    arr(out, 2);
    bulk(out, next.to_string().as_bytes());
    arr(out, keys.len());
    for k in keys {
        bulk(out, &k);
    }
}

/// `RENAME src dst` / `RENAMENX src dst` — reply shapes mirror the
/// server's `Op::Rename` arm.
fn cmd_rename(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>, nx: bool) {
    let name = if nx { "renamenx" } else { "rename" };
    if argv.len() != 3 {
        return wrong_args(out, name);
    }
    let res = if nx { s.renamenx(&argv[1], &argv[2]) } else { s.rename(&argv[1], &argv[2]) };
    match res {
        Ok(true) if nx => int(out, 1),
        Ok(true) => simple(out, "OK"),
        Ok(false) => int(out, 0), // NX: destination exists
        Err(e) => kevy_err(out, &e),
    }
}

/// `COPY src dst [REPLACE]`.
fn cmd_copy(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    let replace = match argv.len() {
        3 => false,
        4 if argv[3].eq_ignore_ascii_case(b"REPLACE") => true,
        4 => return err(out, ERR_SYNTAX),
        _ => return wrong_args(out, "copy"),
    };
    match s.copy(&argv[1], &argv[2], replace) {
        Ok(copied) => int(out, i64::from(copied)),
        Err(e) => kevy_err(out, &e),
    }
}
