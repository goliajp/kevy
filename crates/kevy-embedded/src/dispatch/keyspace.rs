//! The key verbs that span shards: DEL / UNLINK / EXISTS / TOUCH / KEYS /
//! SCAN / RANDOMKEY / RENAME / COPY / TIME / DBSIZE / FLUSHALL. The
//! single-key ones (TYPE, the TTL family, PERSIST) run through `shared`.

use crate::store::Store;

use super::{emit_int, kevy_err, opt_bulk, rest};
use kevy_resp::{
    encode_array_len, encode_bulk, encode_error, encode_integer, encode_simple_string,
};
use kevy_verbs::reply::wrong_args;

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
        b"KEYS" => {
            if argv.len() == 2 {
                let keys = s.keys(Some(&argv[1]), None);
                encode_array_len(out, keys.len() as i64);
                for k in keys {
                    encode_bulk(out, &k);
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
            encode_array_len(out, 2);
            encode_bulk(out, secs.to_string().as_bytes());
            encode_bulk(out, micros.to_string().as_bytes());
        }
        // The server answers DBSIZE / FLUSHALL regardless of extra
        // args — mirror that tolerance.
        b"DBSIZE" => encode_integer(out, s.dbsize() as i64),
        b"FLUSHALL" => match s.flushall() {
            Ok(()) => encode_simple_string(out, "OK"),
            Err(e) => kevy_err(out, &e),
        },
        _ => return false,
    }
    true
}

/// `SCAN cursor [MATCH pattern] [COUNT n] [TYPE type]` — the cursor has
/// the server's layout (shard index above a position within the shard).
fn cmd_scan(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    if argv.len() < 2 {
        return wrong_args(out, "scan");
    }
    let o = match kevy_verbs::args::scan_opts(&super::Args::new(argv)) {
        Ok(o) => o,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let (next, mut keys) = s.scan(o.cursor, o.pattern.as_deref(), o.count);
    if let Some(t) = o.type_filter {
        keys.retain(|k| s.type_of(k).as_bytes() == t.as_slice());
    }
    encode_array_len(out, 2);
    encode_bulk(out, next.to_string().as_bytes());
    encode_array_len(out, keys.len() as i64);
    for k in keys {
        encode_bulk(out, &k);
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
        Ok(true) if nx => encode_integer(out, 1),
        Ok(true) => encode_simple_string(out, "OK"),
        Ok(false) => encode_integer(out, 0), // NX: destination exists
        Err(e) => kevy_err(out, &e),
    }
}

/// `COPY src dst [REPLACE]`.
fn cmd_copy(s: &Store, argv: &[Vec<u8>], out: &mut Vec<u8>) {
    // the grammar refuses a key copied onto itself, as Redis does; the
    // `Store::copy` API method still answers `false` for it
    let replace = match kevy_verbs::multikey::parse_copy(&super::Args::new(argv)) {
        Ok(replace) => replace,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let mode = if replace { crate::CopyMode::Replace } else { crate::CopyMode::IfAbsent };
    match s.copy(&argv[1], &argv[2], mode) {
        Ok(copied) => encode_integer(out, i64::from(copied)),
        Err(e) => kevy_err(out, &e),
    }
}
