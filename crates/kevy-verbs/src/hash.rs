//! Hash commands. The field-TTL family lives in `hash_ttl`.

use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_null_bulk, encode_simple_string,
};
use kevy_store::Store;

use crate::args::{arg_f64, arg_i64, rest_borrowed, scan_match};
use crate::reply::{
    ERR_NOT_FLOAT, ERR_NOT_INT, ERR_SYNTAX, emit_bulk_array, emit_int_result, fmt_score, scan_page,
    store_err, wrong_args,
};
use crate::{Effect, changed, hash_ttl};

/// One hash command; `None` = the verb is not in this group.
// LOC-WAIVER: data-driven verb dispatch table — one arm per hash verb.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    Some(match cmd {
        b"HSET" => {
            hset(store, args, false, out);
            Effect::Write
        }
        // the deprecated alias: HSET's grammar, `+OK` instead of a count
        b"HMSET" => {
            hset(store, args, true, out);
            Effect::Write
        }
        b"HEXPIRE" | b"HPEXPIRE" | b"HPEXPIREAT" | b"HTTL" | b"HPTTL" | b"HPERSIST" => {
            return hash_ttl::exec(cmd, store, args, out);
        }
        b"HSETNX" => {
            if args.len() != 4 {
                wrong_args(out, "hsetnx");
                return Some(Effect::Unchanged);
            }
            let res = store.hsetnx(&args[1], &args[2], &args[3]);
            let set = matches!(res, Ok(true));
            emit_int_result(res.map(i64::from), out);
            changed(set)
        }
        b"HGET" => {
            if args.len() == 3 {
                match store.hget(&args[1], &args[2]) {
                    Ok(Some(v)) => encode_bulk(out, v),
                    Ok(None) => encode_null_bulk(out),
                    Err(e) => store_err(out, e),
                }
            } else {
                wrong_args(out, "hget");
            }
            Effect::Read
        }
        b"HDEL" => {
            if args.len() < 3 {
                wrong_args(out, "hdel");
                return Some(Effect::Unchanged);
            }
            let res = store.hdel(&args[1], &rest_borrowed(args, 2));
            let removed = matches!(res, Ok(n) if n > 0);
            emit_int_result(res.map(|n| n as i64), out);
            changed(removed)
        }
        b"HEXISTS" => {
            if args.len() == 3 {
                emit_int_result(store.hexists(&args[1], &args[2]).map(i64::from), out);
            } else {
                wrong_args(out, "hexists");
            }
            Effect::Read
        }
        b"HLEN" => {
            if args.len() == 2 {
                emit_int_result(store.hlen(&args[1]).map(|n| n as i64), out);
            } else {
                wrong_args(out, "hlen");
            }
            Effect::Read
        }
        b"HINCRBY" => {
            if args.len() != 4 {
                wrong_args(out, "hincrby");
            } else if let Some(d) = arg_i64(&args[3]) {
                emit_int_result(store.hincrby(&args[1], &args[2], d), out);
            } else {
                encode_error(out, ERR_NOT_INT);
            }
            Effect::Write
        }
        b"HINCRBYFLOAT" => {
            if args.len() != 4 {
                wrong_args(out, "hincrbyfloat");
            } else if let Some(d) = arg_f64(&args[3]) {
                match store.hincrbyfloat(&args[1], &args[2], d) {
                    Ok(v) => encode_bulk(out, &fmt_score(v)),
                    Err(e) => store_err(out, e),
                }
            } else {
                encode_error(out, ERR_NOT_FLOAT);
            }
            Effect::Write
        }
        b"HRANDFIELD" => {
            hrandfield(store, args, out);
            Effect::Read
        }
        b"HKEYS" | b"HVALS" | b"HGETALL" => {
            if args.len() != 2 {
                wrong_args(out, &String::from_utf8_lossy(cmd).to_lowercase());
            } else if cmd == b"HKEYS" {
                emit_bulk_array(store.hkeys(&args[1]), out);
            } else if cmd == b"HVALS" {
                emit_bulk_array(store.hvals(&args[1]), out);
            } else {
                emit_bulk_array(store.hgetall(&args[1]), out);
            }
            Effect::Read
        }
        b"HMGET" => {
            hmget(store, args, out);
            Effect::Read
        }
        b"HSCAN" => {
            hscan(store, args, out);
            Effect::Read
        }
        _ => return None,
    })
}

/// `HSET` / `HMSET key field value [field value ...]`. The pair list
/// borrows from argv, so no field or value is copied before the store
/// takes it.
fn hset<A: ArgvView + ?Sized>(store: &mut Store, args: &A, ok_reply: bool, out: &mut Vec<u8>) {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return wrong_args(out, if ok_reply { "hmset" } else { "hset" });
    }
    let pairs: Vec<(&[u8], &[u8])> =
        (2..args.len()).step_by(2).map(|i| (&args[i], &args[i + 1])).collect();
    match store.hset(&args[1], &pairs) {
        Ok(_) if ok_reply => encode_simple_string(out, "OK"),
        res => emit_int_result(res.map(|n| n as i64), out),
    }
}

fn hmget<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "hmget");
    }
    match store.hmget(&args[1], &rest_borrowed(args, 2)) {
        Ok(vals) => {
            encode_array_len(out, vals.len() as i64);
            for v in &vals {
                match v {
                    Some(b) => encode_bulk(out, b),
                    None => encode_null_bulk(out),
                }
            }
        }
        Err(e) => store_err(out, e),
    }
}

/// `HRANDFIELD key [count [WITHVALUES]]`: one field as a bulk without a
/// count; with one, an array of fields, or field/value pairs flattened.
fn hrandfield<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 2 || args.len() > 4 {
        return wrong_args(out, "hrandfield");
    }
    if args.len() == 2 {
        return match store.hrandfield(&args[1], 1) {
            Ok(v) if v.is_empty() => encode_null_bulk(out),
            Ok(v) => encode_bulk(out, &v[0]),
            Err(e) => store_err(out, e),
        };
    }
    let Some(count) = arg_i64(&args[2]) else {
        return encode_error(out, ERR_NOT_INT);
    };
    let with_values = args.len() == 4;
    if with_values && !args[3].eq_ignore_ascii_case(b"WITHVALUES") {
        return encode_error(out, ERR_SYNTAX);
    }
    if with_values {
        match store.hrandfield_with_values(&args[1], count) {
            Err(e) => store_err(out, e),
            Ok(items) => {
                encode_array_len(out, (items.len() * 2) as i64);
                for (f, v) in &items {
                    encode_bulk(out, f);
                    encode_bulk(out, v);
                }
            }
        }
    } else {
        match store.hrandfield(&args[1], count) {
            Err(e) => store_err(out, e),
            Ok(fields) => {
                encode_array_len(out, fields.len() as i64);
                for f in &fields {
                    encode_bulk(out, f);
                }
            }
        }
    }
}

/// `HSCAN key cursor [MATCH pattern] [COUNT n]` — every pair in one
/// batch, field then value.
fn hscan<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "hscan");
    }
    if arg_i64(&args[2]).is_none() {
        return encode_error(out, ERR_NOT_INT);
    }
    let Some(pat) = scan_match(args, 3) else {
        return encode_error(out, ERR_SYNTAX);
    };
    match store.hgetall(&args[1]) {
        Err(e) => store_err(out, e),
        Ok(flat) => {
            let mut page: Vec<Vec<u8>> = Vec::with_capacity(flat.len());
            for [field, value] in flat.as_chunks::<2>().0 {
                if pat.as_ref().is_none_or(|p| kevy_store::glob_match(p, field)) {
                    page.push(field.clone());
                    page.push(value.clone());
                }
            }
            scan_page(out, &page);
        }
    }
}
