//! Command helpers shared by the dispatcher.

use kevy_resp::{encode_array_len, encode_bulk, encode_error, encode_integer};

pub(crate) use kevy_verbs::args::{arg_f64, arg_i64, upper_verb};
pub(crate) use kevy_verbs::reply::{OOM_ERR, fmt_score, store_err, wrong_args};

/// Nothing in the local dispatch chain handled this verb — say which of
/// the two reasons that is.
///
/// A verb the metadata table knows is not an unknown command: it is a known
/// one called with the wrong count, which the extension routes in
/// `cmd_resolve` guard (`IDX.QUERY if args.len() >= 4`) and let fall
/// through to here when the guard misses. Twelve of fourteen guarded verbs
/// answered a wrong-arity call with "unknown command 'IDX.QUERY'" —
/// telling the caller a command that exists does not — until this site
/// started consulting the arity `verb_meta` had all along.
///
/// Redis's convention, and this engine's own everywhere else: negative
/// arity is a minimum, positive is exact.
pub(crate) fn unhandled_verb(out: &mut Vec<u8>, name: &[u8], nargs: usize) {
    let mut buf = [0u8; 32];
    let upper = upper_verb(name, &mut buf);
    if let Some(meta) = std::str::from_utf8(upper).ok().and_then(crate::verb_meta::verb_meta) {
        let (n, a) = (nargs as i64, i64::from(meta.arity));
        if (a < 0 && n < -a) || (a > 0 && n != a) {
            return wrong_args(out, &meta.name.to_lowercase());
        }
    }
    let shown = String::from_utf8_lossy(name);
    encode_error(out, &format!("ERR unknown command '{shown}'"));
}

/// `HELLO` — RESP2 server-info handshake (a flat field/value array). We always
/// report `proto 2`; switching to a true RESP3 reply encoding is deferred.
pub(crate) fn cmd_hello(out: &mut Vec<u8>) {
    encode_array_len(out, 14);
    encode_bulk(out, b"server");
    encode_bulk(out, b"kevy");
    encode_bulk(out, b"version");
    encode_bulk(out, env!("CARGO_PKG_VERSION").as_bytes());
    encode_bulk(out, b"proto");
    encode_integer(out, 2);
    encode_bulk(out, b"id");
    encode_integer(out, 0);
    encode_bulk(out, b"mode");
    encode_bulk(out, b"standalone");
    encode_bulk(out, b"role");
    encode_bulk(out, b"master");
    encode_bulk(out, b"modules");
    encode_array_len(out, 0);
}

/// Verb classification tables (`is_write_verb` / `notify_class_for_verb` /
/// `is_growing_write_verb`) live in [`crate::cmd_class`]; re-exported here
/// so dispatchers keep their `cmd::*` paths.
pub(crate) use crate::cmd_class::{is_growing_write_verb, is_write_verb, notify_class_for_verb};
