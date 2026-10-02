//! `GEORADIUS` / `GEORADIUSBYMEMBER` (and their `_RO` read-only twins).
//! Deprecated by Redis in favour of `GEOSEARCH`/`GEOSEARCHSTORE` but
//! still widely used by client libraries. Both translate the legacy
//! "fixed prefix then flag soup" form into the structured `Opts` the
//! search core consumes, then either emit the GEOSEARCH-style reply
//! or perform a STORE / STOREDIST write into a destination ZSet.

use kevy_resp::{ArgvView, RespVersion, encode_integer};
use kevy_store::Store;

use crate::Effect;
use crate::args::upper_verb;
use crate::reply::store_err;

use super::search;
use super::search::{Form, RadiusReply};

/// `GEORADIUS[_RO] key lon lat radius unit [...]` — legacy.
pub(super) fn cmd_georadius<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    read_only: bool,
    proto: RespVersion,
) -> Effect {
    run_radius(store, args, out, Form::Radius { read_only }, proto)
}

/// `GEORADIUSBYMEMBER[_RO] key member radius unit [...]` — legacy.
pub(super) fn cmd_georadiusbymember<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    read_only: bool,
    proto: RespVersion,
) -> Effect {
    run_radius(store, args, out, Form::ByMember { read_only }, proto)
}

/// `Write` only when the query stored its hits into a destination.
fn run_radius<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    form: Form,
    proto: RespVersion,
) -> Effect {
    let mut q = match search::plan(store, args, form) {
        Ok(q) => q,
        Err(e) => {
            e.emit(&args[0], out);
            return Effect::Read;
        }
    };
    q.opts.proto = proto;
    let hits = match search::run_search(store, &q) {
        Ok(h) => h,
        Err(e) => {
            store_err(out, e);
            return Effect::Read;
        }
    };
    let had_dst = q.store_dst.as_ref().is_some_and(|d| store.key_exists(d));
    match search::emit_or_store(out, store, &hits, &q) {
        RadiusReply::Replied => Effect::Read,
        RadiusReply::Stored(n) => {
            encode_integer(out, n as i64);
            crate::changed(n > 0 || had_dst)
        }
    }
}

/// The destination key of a legacy `STORE` / `STOREDIST` option, if any.
/// Walks the option tail with the arities the parser uses, so a COUNT
/// value or a member that happens to spell "STORE" can't be mistaken for
/// the token; the last one wins, as it does in the parser. `None` = no
/// STORE (a plain query) or a syntax error the dispatch path will report —
/// either way there is no destination to route to.
pub(super) fn legacy_store_dst<A: ArgvView + ?Sized>(args: &A, start: usize) -> Option<Vec<u8>> {
    let mut dst = None;
    let mut i = start;
    let mut buf = [0u8; 32];
    while i < args.len() {
        i += match upper_verb(&args[i], &mut buf) {
            b"STORE" | b"STOREDIST" => {
                dst = Some(args.get(i + 1)?.to_vec());
                2
            }
            b"ASC" | b"DESC" | b"WITHCOORD" | b"WITHDIST" | b"WITHHASH" | b"ANY" => 1,
            b"COUNT" => 2,
            _ => return None,
        };
    }
    dst
}
