//! `GEORADIUS` / `GEORADIUSBYMEMBER` (and their `_RO` read-only twins).
//! Deprecated by Redis in favour of `GEOSEARCH`/`GEOSEARCHSTORE` but
//! still widely used by client libraries. Both translate the legacy
//! "fixed prefix then flag soup" form into the structured `Opts` the
//! search core consumes, then either emit the GEOSEARCH-style reply
//! or perform a STORE / STOREDIST write into a destination ZSet.

use kevy_resp::{ArgvView, CmdError, encode_error, encode_integer};
use kevy_store::Store;

use crate::cmd::{arg_f64, store_err};

use super::parse_unit;
use super::search;
use super::search::{Anchor, LegacyRadiusParsed, RadiusReply, SearchError};

/// `GEORADIUS key lon lat radius unit [...]` — legacy.
pub(super) fn cmd_georadius<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    read_only: bool,
) {
    run_radius(store, out, plan_radius(args, false), read_only);
}

/// `GEORADIUSBYMEMBER key member radius unit [...]` — legacy.
pub(super) fn cmd_georadiusbymember<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    read_only: bool,
) {
    run_radius(store, out, plan_radius(args, true), read_only);
}

/// Parse a legacy `GEORADIUS[BYMEMBER]` argv into `(source key, options +
/// STORE destination)`. Shared by the dispatch path below and by the
/// cross-shard search half (`super::store::geo_search`), which runs on the
/// source's shard and must read exactly the same query out of the argv.
pub(super) fn plan_radius<A: ArgvView + ?Sized>(
    args: &A,
    bymember: bool,
) -> Result<(Vec<u8>, LegacyRadiusParsed), CmdError> {
    let (anchor, radius_idx) = if bymember {
        if args.len() < 5 {
            return Err(CmdError::Wire(
                "ERR wrong number of arguments for 'georadiusbymember' command",
            ));
        }
        (Anchor::Member(args[2].to_vec()), 3)
    } else {
        if args.len() < 6 {
            return Err(CmdError::Wire("ERR wrong number of arguments for 'georadius' command"));
        }
        let lon = arg_f64(&args[2]).ok_or("ERR value is not a valid float")?;
        let lat = arg_f64(&args[3]).ok_or("ERR value is not a valid float")?;
        (Anchor::LonLat(lon, lat), 4)
    };
    let radius = arg_f64(&args[radius_idx]).ok_or("ERR value is not a valid float")?;
    let unit = parse_unit(&args[radius_idx + 1])
        .ok_or("ERR unsupported unit provided. please use M, KM, FT, MI")?;
    let parsed = search::parse_legacy_radius(args, radius_idx + 2, anchor, radius * unit, unit)?;
    Ok((args[1].to_vec(), parsed))
}

fn run_radius(
    store: &mut Store,
    out: &mut Vec<u8>,
    planned: Result<(Vec<u8>, LegacyRadiusParsed), CmdError>,
    read_only: bool,
) {
    let (key, parsed) = match planned {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    if read_only && parsed.store_dst.is_some() {
        return encode_error(out, "ERR can't store result in the _RO variant");
    }
    let hits = match search::run_search(store, &key, &parsed.opts) {
        Ok(h) => h,
        Err(SearchError::NoMember) => {
            return encode_error(out, "ERR could not decode requested zset member");
        }
        Err(SearchError::Store(e)) => return store_err(out, e),
    };
    match search::emit_or_store(out, store, &hits, &parsed) {
        RadiusReply::Replied => {}
        RadiusReply::Stored(n) => encode_integer(out, n as i64),
    }
}

/// The destination key of a legacy `STORE` / `STOREDIST` option, if any.
/// Walks the option tail with the same arities [`search::parse_legacy_radius`]
/// uses, so a COUNT value or a member that happens to spell "STORE" can't be
/// mistaken for the token; the last one wins, as it does in the parser.
/// `None` = no STORE (a plain query) or a syntax error the dispatch path will
/// report — either way there is no destination to route to.
pub(super) fn legacy_store_dst<A: ArgvView + ?Sized>(args: &A, start: usize) -> Option<Vec<u8>> {
    let mut dst = None;
    let mut i = start;
    while i < args.len() {
        let step = match args[i].to_ascii_uppercase().as_slice() {
            b"STORE" | b"STOREDIST" => {
                dst = Some(args.get(i + 1)?.to_vec());
                2
            }
            b"ASC" | b"DESC" | b"WITHCOORD" | b"WITHDIST" | b"WITHHASH" => 1,
            b"COUNT" => {
                if args.get(i + 2).is_some_and(|a| a.eq_ignore_ascii_case(b"ANY")) {
                    3
                } else {
                    2
                }
            }
            _ => return None,
        };
        i += step;
    }
    dst
}
