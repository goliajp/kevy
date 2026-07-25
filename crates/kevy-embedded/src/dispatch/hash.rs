//! Hash-family verbs, including the Redis 7.4 field-TTL family
//! (`HEXPIRE` / `HPEXPIRE` / `HPEXPIREAT` / `HTTL` / `HPTTL` /
//! `HPERSIST`) and `HSCAN`.

use crate::store::Store;

use kevy_store::{now_unix_ms, HExpireCond};

use super::util::{
    arg_f64, arg_i64, arr, bulk, emit_bulk_array, emit_int, err, fmt_score, int, kevy_err,
    opt_bulk, rest, wrong_args, ERR_NOT_FLOAT, ERR_NOT_INT, ERR_SYNTAX,
};

/// One hash-family request; `false` = verb not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per hash verb.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    match up {
        b"HSET" => {
            if argv.len() < 4 || !argv.len().is_multiple_of(2) {
                wrong_args(out, "hset");
            } else {
                let pairs: Vec<(&[u8], &[u8])> = (2..argv.len())
                    .step_by(2)
                    .map(|i| (argv[i].as_slice(), argv[i + 1].as_slice()))
                    .collect();
                emit_int(out, s.hset(&argv[1], &pairs).map(|n| n as i64));
            }
        }
        b"HSETNX" => {
            if argv.len() == 4 {
                match s.hsetnx(&argv[1], &argv[2], &argv[3]) {
                    Ok(set) => int(out, i64::from(set)),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                wrong_args(out, "hsetnx");
            }
        }
        b"HGET" => {
            if argv.len() == 3 {
                match s.hget(&argv[1], &argv[2]) {
                    Ok(v) => opt_bulk(out, v),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                wrong_args(out, "hget");
            }
        }
        b"HDEL" => {
            if argv.len() < 3 {
                wrong_args(out, "hdel");
            } else {
                emit_int(out, s.hdel(&argv[1], &rest(argv, 2)).map(|n| n as i64));
            }
        }
        b"HEXISTS" => {
            if argv.len() == 3 {
                emit_int(out, s.hexists(&argv[1], &argv[2]).map(i64::from));
            } else {
                wrong_args(out, "hexists");
            }
        }
        b"HLEN" => {
            if argv.len() == 2 {
                emit_int(out, s.hlen(&argv[1]).map(|n| n as i64));
            } else {
                wrong_args(out, "hlen");
            }
        }
        b"HINCRBY" => {
            if argv.len() != 4 {
                wrong_args(out, "hincrby");
            } else if let Some(d) = arg_i64(&argv[3]) {
                emit_int(out, s.hincrby(&argv[1], &argv[2], d));
            } else {
                err(out, ERR_NOT_INT);
            }
        }
        b"HINCRBYFLOAT" => {
            if argv.len() != 4 {
                wrong_args(out, "hincrbyfloat");
            } else if let Some(d) = arg_f64(&argv[3]) {
                match s.hincrbyfloat(&argv[1], &argv[2], d) {
                    Ok(v) => bulk(out, &fmt_score(v)),
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                err(out, ERR_NOT_FLOAT);
            }
        }
        b"HKEYS" => {
            if argv.len() == 2 {
                emit_bulk_array(out, s.hkeys(&argv[1]));
            } else {
                wrong_args(out, "hkeys");
            }
        }
        b"HVALS" => {
            if argv.len() == 2 {
                emit_bulk_array(out, s.hvals(&argv[1]));
            } else {
                wrong_args(out, "hvals");
            }
        }
        b"HGETALL" => {
            if argv.len() == 2 {
                match s.hgetall(&argv[1]) {
                    Ok(pairs) => {
                        arr(out, pairs.len() * 2);
                        for (f, v) in pairs {
                            bulk(out, &f);
                            bulk(out, &v);
                        }
                    }
                    Err(e) => kevy_err(out, &e),
                }
            } else {
                wrong_args(out, "hgetall");
            }
        }
        b"HMGET" => {
            if argv.len() < 3 {
                wrong_args(out, "hmget");
            } else {
                match s.hmget(&argv[1], &rest(argv, 2)) {
                    Ok(vals) => {
                        arr(out, vals.len());
                        for v in vals {
                            opt_bulk(out, v);
                        }
                    }
                    Err(e) => kevy_err(out, &e),
                }
            }
        }
        b"HSCAN" => cmd_hscan(s, argv, out),
        b"HEXPIRE" => cmd_hexpire(s, argv, out, "hexpire", |n| {
            now_unix_ms().saturating_add_signed(n.saturating_mul(1000))
        }),
        b"HPEXPIRE" => {
            cmd_hexpire(s, argv, out, "hpexpire", |n| now_unix_ms().saturating_add_signed(n));
        }
        b"HPEXPIREAT" => cmd_hexpire(s, argv, out, "hpexpireat", |n| n.max(0) as u64),
        b"HTTL" => cmd_httl(s, argv, true, "httl", out),
        b"HPTTL" => cmd_httl(s, argv, false, "hpttl", out),
        b"HPERSIST" => cmd_hpersist(s, argv, out),
        _ => return false,
    }
    true
}

/// `HSCAN key cursor [MATCH pattern] [COUNT n]` — one full batch,
/// cursor "0" (the server's small-collection SCAN shape).
fn cmd_hscan(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 3 {
        return wrong_args(out, "hscan");
    }
    if arg_i64(&argv[2]).is_none() {
        return err(out, ERR_NOT_INT);
    }
    let Some(pat) = super::parse_match_count(argv, 3) else {
        return err(out, ERR_SYNTAX);
    };
    match s.hgetall(&argv[1]) {
        Err(e) => kevy_err(out, &e),
        Ok(pairs) => {
            let mut flat: Vec<Vec<u8>> = Vec::with_capacity(pairs.len() * 2);
            for (f, v) in pairs {
                if pat.as_ref().is_none_or(|p| kevy_store::glob_match(p, &f)) {
                    flat.push(f);
                    flat.push(v);
                }
            }
            super::emit_scan_page(out, b"0", &flat);
        }
    }
}

/// `[NX|XX|GT|LT] FIELDS n f…` tail starting at `i` — the server's
/// `parse_cond_fields`, including its exact error wording.
fn parse_cond_fields(
    argv: &[Vec<u8>],
    mut i: usize,
) -> Result<(HExpireCond, Vec<usize>), &'static str> {
    let mut cond = HExpireCond::Always;
    if let Some(a) = argv.get(i) {
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
    if !argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(b"FIELDS")) {
        return Err("ERR Mandatory keyword FIELDS is missing or not at the right position");
    }
    i += 1;
    let n: usize = argv
        .get(i)
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .ok_or("ERR Parameter `numFields` should be greater than 0")?;
    i += 1;
    if argv.len() != i + n {
        return Err("ERR Parameter `numFields` is more than number of arguments");
    }
    Ok((cond, (i..i + n).collect()))
}

fn emit_codes(out: &mut Vec<u8>, codes: &[i8]) {
    arr(out, codes.len());
    for c in codes {
        int(out, i64::from(*c));
    }
}

/// Shared body for the three deadline forms; `to_abs_ms` converts the
/// raw argument into an absolute unix-ms deadline.
fn cmd_hexpire(
    s: &Store,
    argv: &[Vec<u8>],
    out: &mut Vec<u8>,
    name: &str,
    to_abs_ms: impl Fn(i64) -> u64,
) {
    if argv.len() < 6 {
        return wrong_args(out, name);
    }
    let Some(raw) = arg_i64(&argv[2]) else {
        return err(out, ERR_NOT_INT);
    };
    let (cond, idx) = match parse_cond_fields(argv, 3) {
        Ok(t) => t,
        Err(e) => return err(out, e),
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| argv[i].as_slice()).collect();
    match s.hpexpire_at(&argv[1], &fields, to_abs_ms(raw), cond) {
        Ok(codes) => emit_codes(out, &codes),
        Err(e) => kevy_err(out, &e),
    }
}

fn cmd_httl(s: &Store, argv: &[Vec<u8>], in_secs: bool, name: &str, out: &mut Vec<u8>) {
    if argv.len() < 5 {
        return wrong_args(out, name);
    }
    let (_, idx) = match parse_cond_fields(argv, 2) {
        Ok(t) => t,
        Err(e) => return err(out, e),
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| argv[i].as_slice()).collect();
    match s.hpttl(&argv[1], &fields) {
        Err(e) => kevy_err(out, &e),
        Ok(ttls) => {
            arr(out, ttls.len());
            for ms in ttls {
                // -2 / -1 sentinels pass through untouched.
                int(out, if in_secs && ms >= 0 { (ms + 500) / 1000 } else { ms });
            }
        }
    }
}

fn cmd_hpersist(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 5 {
        return wrong_args(out, "hpersist");
    }
    let (_, idx) = match parse_cond_fields(argv, 2) {
        Ok(t) => t,
        Err(e) => return err(out, e),
    };
    let fields: Vec<&[u8]> = idx.iter().map(|&i| argv[i].as_slice()).collect();
    match s.hpersist(&argv[1], &fields) {
        Ok(codes) => emit_codes(out, &codes),
        Err(e) => kevy_err(out, &e),
    }
}
