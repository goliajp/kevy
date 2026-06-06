//! RESP3-shape reply overrides — extracted from [`crate::dispatch`] to
//! keep that file under the 500-LOC house rule.
//!
//! Spec-legal gradual migration: each command listed here gets a RESP3
//! reply (Map / Set / Double / Verbatim / …); everything else keeps its
//! V2 wire on a RESP3 connection until a sibling arm gets added. The
//! caller in `dispatch_with_proto` runs this chain BEFORE the V2 chain
//! and short-circuits on a hit, so adding an override is a 1:1 swap
//! from a V2 helper to a RESP3 helper.

use crate::cmd::*;
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
pub(crate) fn try_resp3_overrides<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"HGETALL" => {
            if args.len() != 2 {
                wrong_args(out, "hgetall");
            } else {
                emit_hash_map_resp3(store.hgetall(&args[1]), out);
            }
            true
        }
        b"ZSCORE" => {
            if args.len() != 3 {
                wrong_args(out, "zscore");
            } else {
                emit_zscore_resp3(store.zscore(&args[1], &args[2]), out);
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
            if args.len() != 2 {
                wrong_args(out, "smembers");
            } else {
                emit_set_resp3(store.smembers(&args[1]), out);
            }
            true
        }
        b"CONFIG" => {
            // CONFIG GET shape changes RESP2 `*2N` array → RESP3 `%N` Map.
            // Other CONFIG subcommands (SET / REWRITE / RESETSTAT) have
            // the same reply shape under both protos; cmd_config ignores
            // `proto` for those arms. Routing all CONFIG sub-cmds through
            // the V3 path here is simpler than peeking the sub-cmd.
            let cfg = crate::config_global::get();
            crate::ops::config::cmd_config(&cfg, args, out, RespVersion::V3);
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
            let cfg = crate::config_global::get();
            crate::ops::cmd_info(&cfg, store, args, out, RespVersion::V3);
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
