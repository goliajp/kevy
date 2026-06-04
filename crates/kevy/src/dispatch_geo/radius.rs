//! `GEORADIUS` / `GEORADIUSBYMEMBER` (and their `_RO` read-only twins).
//! Deprecated by Redis in favour of `GEOSEARCH`/`GEOSEARCHSTORE` but
//! still widely used by client libraries. Both translate the legacy
//! "fixed prefix then flag soup" form into the structured `Opts` the
//! search core consumes, then either emit the GEOSEARCH-style reply
//! or perform a STORE / STOREDIST write into a destination ZSet.

use kevy_resp::{ArgvView, encode_error, encode_integer};
use kevy_store::Store;

use crate::cmd::{arg_f64, store_err, wrong_args};

use super::parse_unit;
use super::search;
use super::search::{Anchor, RadiusReply, SearchError};

/// `GEORADIUS key lon lat radius unit [...]` — legacy.
pub(super) fn cmd_georadius<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    read_only: bool,
) {
    if args.len() < 6 {
        return wrong_args(out, "georadius");
    }
    let lon = match arg_f64(&args[2]) {
        Some(v) => v,
        None => return encode_error(out, "ERR value is not a valid float"),
    };
    let lat = match arg_f64(&args[3]) {
        Some(v) => v,
        None => return encode_error(out, "ERR value is not a valid float"),
    };
    finish_radius(store, args, out, Anchor::LonLat(lon, lat), 4, read_only);
}

/// `GEORADIUSBYMEMBER key member radius unit [...]` — legacy.
pub(super) fn cmd_georadiusbymember<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    read_only: bool,
) {
    if args.len() < 5 {
        return wrong_args(out, "georadiusbymember");
    }
    let anchor = Anchor::Member(args[2].to_vec());
    finish_radius(store, args, out, anchor, 3, read_only);
}

fn finish_radius<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    anchor: Anchor,
    radius_idx: usize,
    read_only: bool,
) {
    let radius = match arg_f64(&args[radius_idx]) {
        Some(v) => v,
        None => return encode_error(out, "ERR value is not a valid float"),
    };
    let unit = match parse_unit(&args[radius_idx + 1]) {
        Some(u) => u,
        None => {
            return encode_error(
                out,
                "ERR unsupported unit provided. please use M, KM, FT, MI",
            );
        }
    };
    let parsed = match search::parse_legacy_radius(args, radius_idx + 2, anchor, radius * unit, unit) {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg),
    };
    if read_only && parsed.store_dst.is_some() {
        return encode_error(out, "ERR can't store result in the _RO variant");
    }
    let key = args[1].to_vec();
    let hits = match search::run_search(store, &key, &parsed.opts) {
        Ok(h) => h,
        Err(SearchError::NoMember) => {
            return encode_error(out, "ERR could not decode requested zset member");
        }
        Err(SearchError::Store(e)) => return store_err(out, e),
    };
    match search::emit_or_store(out, store, &hits, &parsed) {
        RadiusReply::Replied => {}
        RadiusReply::Stored(Ok(n)) => encode_integer(out, n as i64),
        RadiusReply::Stored(Err(e)) => store_err(out, e),
    }
}
