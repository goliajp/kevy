//! RESP3-shape reply overrides — extracted from [`crate::dispatch`] to
//! keep that file under the 500-LOC house rule.
//!
//! Spec-legal gradual migration: each command listed here gets a RESP3
//! reply (Map / Set / Double / Verbatim / …); everything else keeps its
//! V2 wire on a RESP3 connection until a sibling arm gets added. The
//! caller in `dispatch_with_proto` runs this chain BEFORE the V2 chain
//! and short-circuits on a hit, so adding an override is a 1:1 swap
//! from a V2 helper to a RESP3 helper.

use crate::cmd::{arg_f64, arg_i64, cmd_zrange, cmd_zrangebyscore, store_err, wrong_args};
use crate::state::Ctx;
use kevy_resp::{
    ArgvView, RespVersion, encode_bulk, encode_double, encode_error, encode_map_header,
    encode_null, encode_set_header,
};
use kevy_store::{Store, StoreError};

/// RESP3-shape replies for the commands whose `dispatch_into` output
/// differs from the V2 form. Returns `true` if the cmd matched + the
/// reply was emitted (so the caller skips the V2 chain).
///
/// Adding a new override here is the P3-style migration point: each
/// arm is a 1:1 swap from a V2 helper to a RESP3 helper (Map / Set /
/// Double / Verbatim / …). All other commands keep their V2 wire on
/// RESP3 conns until they get an override — spec-legal gradual
/// migration.
// LOC-WAIVER: data-driven RESP3-override verb table — one arm per shape-changing verb.
pub(crate) fn try_resp3_overrides<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        // Found by bench/resp3gate.sh, which asks the pinned redis which
        // verbs change shape under HELLO 3 rather than trusting this table
        // to be complete. It was not: five verbs sent the RESP2 wire to a
        // client that had negotiated RESP3, while the site said in three
        // languages that a client library would not notice.
        b"HRANDFIELD" if args.len() == 4 && args[3].eq_ignore_ascii_case(b"WITHVALUES") => {
            // RESP3 nests each field with its value; RESP2 flattens them.
            match arg_i64(&args[2]) {
                Some(count) => match store.hrandfield(&args[1], count, true) {
                    Ok(items) => {
                        kevy_resp::encode_array_len(out, items.len() as i64);
                        for (f, v) in &items {
                            kevy_resp::encode_array_len(out, 2);
                            encode_bulk(out, f);
                            encode_bulk(out, v);
                        }
                    }
                    Err(e) => store_err(out, e),
                },
                None => encode_error(out, "ERR value is not an integer or out of range"),
            }
            true
        }
        b"ZADD" => {
            // Only the INCR form changes shape: it returns the new score,
            // which is a Double in RESP3 and a bulk string in RESP2. Plain
            // ZADD returns an integer in both, so it falls through.
            match crate::cmd_zadd::parse_zadd_flags(args) {
                Ok((flags, true, first)) if args.len() == first + 2 => {
                    match arg_f64(&args[first]) {
                        Some(delta) => emit_zadd_incr_resp3(
                            store.zadd_incr(&args[1], delta, &args[first + 1], flags),
                            out,
                        ),
                        None => encode_error(out, "ERR value is not a valid float"),
                    }
                    true
                }
                _ => false,
            }
        }
        b"ZPOPMIN" => {
            if (2..=3).contains(&args.len()) {
                let count = if args.len() == 3 {
                    match arg_i64(&args[2]) {
                        Some(c) if c >= 0 => c as usize,
                        Some(_) => {
                            encode_error(out, "ERR value is out of range, must be positive");
                            return true;
                        }
                        None => {
                            encode_error(out, "ERR value is not an integer or out of range");
                            return true;
                        }
                    }
                } else {
                    1
                };
                emit_zpopmin_resp3(store.zpopmin(&args[1], count), out);
            } else {
                wrong_args(out, "zpopmin");
            }
            true
        }
        b"SPOP" if args.len() == 3 => {
            // Only the counted form: `SPOP key` is a single bulk in both
            // protocols, `SPOP key N` is an array in RESP2 and a Set in RESP3.
            match arg_i64(&args[2]) {
                Some(c) if c >= 0 => emit_spop_set_resp3(store.spop(&args[1], c as usize), out),
                Some(_) => encode_error(out, "ERR value is out of range, must be positive"),
                None => encode_error(out, "ERR value is not an integer or out of range"),
            }
            true
        }
        b"GEOPOS" if args.len() >= 3 => {
            emit_geopos_resp3(ctx, store, args, out);
            true
        }
        b"HGETALL" => {
            if args.len() == 2 {
                emit_hash_map_resp3(store.hgetall(&args[1]), out);
            } else {
                wrong_args(out, "hgetall");
            }
            true
        }
        b"ZSCORE" => {
            if args.len() == 3 {
                emit_zscore_resp3(store.zscore(&args[1], &args[2]), out);
            } else {
                wrong_args(out, "zscore");
            }
            true
        }
        b"ZINCRBY" => {
            if args.len() != 4 {
                wrong_args(out, "zincrby");
            } else if let Some(incr) = arg_f64(&args[2]) {
                emit_zincrby_resp3(store.zincrby(&args[1], incr, &args[3]), out);
            } else {
                encode_error(out, "ERR value is not a valid float");
            }
            true
        }
        b"SMEMBERS" => {
            if args.len() == 2 {
                emit_set_resp3(store.smembers(&args[1]), out);
            } else {
                wrong_args(out, "smembers");
            }
            true
        }
        b"CONFIG" => {
            // CONFIG GET shape changes RESP2 `*2N` array → RESP3 `%N` Map.
            // Other CONFIG subcommands (SET / REWRITE / RESETSTAT) have
            // the same reply shape under both protos; cmd_config ignores
            // `proto` for those arms. Routing all CONFIG sub-cmds through
            // the V3 path here is simpler than peeking the sub-cmd.
            crate::ops::config::cmd_config(ctx, args, out, RespVersion::V3);
            true
        }
        // ZRANGE WITHSCORES + ZRANGEBYSCORE WITHSCORES: V3 emits an
        // array of [member, score] 2-element nested arrays (each score
        // a Double `,N`), vs the V2 flat interleaved bulk array. The
        // no-WITHSCORES form is the same plain `*N` array of bulks on
        // both protos (cmd_zrange handles that branch internally).
        b"ZRANGE" => {
            cmd_zrange(store, args, out, RespVersion::V3);
            true
        }
        b"ZRANGEBYSCORE" => {
            cmd_zrangebyscore(store, args, out, RespVersion::V3);
            true
        }
        // RESP3 carries multi-line text replies as Verbatim strings
        // (`=N\r\ntxt:<body>\r\n`) so the client knows the body is
        // human-readable text (no JSON / table parsing). V2 stays as
        // plain bulk. INFO and CLIENT INFO / LIST are the kevy verbs
        // whose body is unambiguously text.
        b"INFO" => {
            crate::ops::cmd_info(ctx, store, args, out, RespVersion::V3);
            true
        }
        b"CLIENT" => {
            crate::ops::client::cmd_client(args, out, RespVersion::V3);
            true
        }
        _ => false,
    }
}

/// `HGETALL` over RESP3: flat `[k, v, k, v, ...]` shape from the store
/// becomes a `%N` Map header + N (k, v) pairs.
fn emit_hash_map_resp3(res: Result<Vec<Vec<u8>>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(flat) => {
            let pairs = flat.len() / 2;
            encode_map_header(out, pairs as i64);
            for v in &flat {
                encode_bulk(out, v);
            }
        }
        Err(e) => store_err(out, e),
    }
}

/// `SMEMBERS` over RESP3: array of bulk strings becomes a `~N` Set header.
fn emit_set_resp3(res: Result<Vec<Vec<u8>>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(items) => {
            encode_set_header(out, items.len() as i64);
            for v in &items {
                encode_bulk(out, v);
            }
        }
        Err(e) => store_err(out, e),
    }
}

/// `ZSCORE` over RESP3: `Some(f)` → `,<f>\r\n` Double; `None` →
/// `_\r\n` RESP3 Null (vs the RESP2 `$-1\r\n` nil bulk).
fn emit_zscore_resp3(res: Result<Option<f64>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(Some(sc)) => encode_double(out, sc),
        Ok(None) => encode_null(out),
        Err(e) => store_err(out, e),
    }
}

/// `ZINCRBY` over RESP3: new score → Double (RESP2 emitted bulk).
fn emit_zincrby_resp3(res: Result<f64, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(sc) => encode_double(out, sc),
        Err(e) => store_err(out, e),
    }
}

/// `ZPOPMIN` over RESP3: scores are Doubles (RESP2 emits bulk strings).
fn emit_zpopmin_resp3(res: Result<Vec<(Vec<u8>, f64)>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(items) => {
            kevy_resp::encode_array_len(out, (items.len() * 2) as i64);
            for (m, sc) in &items {
                encode_bulk(out, m);
                encode_double(out, *sc);
            }
        }
        Err(e) => store_err(out, e),
    }
}

/// `SPOP key count` over RESP3: a Set, not an Array.
fn emit_spop_set_resp3(res: Result<Vec<Vec<u8>>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(members) => {
            encode_set_header(out, members.len() as i64);
            for m in &members {
                encode_bulk(out, m);
            }
        }
        Err(e) => store_err(out, e),
    }
}

/// `GEOPOS` over RESP3: coordinates are Doubles (RESP2 emits bulk strings).
fn emit_geopos_resp3<A: ArgvView + ?Sized>(
    _ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    let n = args.len() - 2;
    // Same as the V2 path: the type must resolve before the header is written.
    if let Err(e) = store.zscore(&args[1], &args[2]) {
        return store_err(out, e);
    }
    kevy_resp::encode_array_len(out, n as i64);
    for i in 0..n {
        match store.zscore(&args[1], &args[i + 2]) {
            Ok(Some(score)) => {
                let (lon, lat) = kevy_geo::decode_score(score);
                kevy_resp::encode_array_len(out, 2);
                encode_double(out, lon);
                encode_double(out, lat);
            }
            Ok(None) => kevy_resp::encode_array_len(out, -1),
            Err(e) => return store_err(out, e),
        }
    }
}

/// `ZADD … INCR` over RESP3: the new score is a Double (RESP2 emits bulk).
fn emit_zadd_incr_resp3(res: Result<Option<f64>, StoreError>, out: &mut Vec<u8>) {
    match res {
        Ok(Some(next)) => encode_double(out, next),
        Ok(None) => encode_null(out),
        Err(e) => store_err(out, e),
    }
}
