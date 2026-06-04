//! `GEOADD` / `GEOPOS` / `GEODIST` / `GEOHASH` / `GEOSEARCH` — the
//! Redis GEO command family. Geo data is stored in a regular `ZSet`
//! keyed by member with a 52-bit interleaved-geohash score (the same
//! wire encoding Redis uses), so we layer entirely on the existing
//! `Store::zadd` / `zscore` / `zrange_by_score` API — no new value
//! variant.
//!
//! Sub-module layout:
//! - `mod.rs` — dispatch table + the four basic commands (GEOADD,
//!   GEOPOS, GEODIST, GEOHASH).
//! - `search.rs` — GEOSEARCH, by far the largest single command in
//!   this family (radius/box modes, six option flags). Split out so
//!   each file stays under the project's ≤500-LOC rule.

mod search;

use kevy_geo::{
    decode_score, encode_base32_geohash, encode_score, haversine_meters,
};
use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_null_bulk,
};
use kevy_store::Store;

use crate::cmd::{arg_f64, store_err, wrong_args};

/// Dispatch table for the geo verbs. Returns `true` if the command was
/// recognised (and a reply has been written to `out`).
pub(crate) fn dispatch_geo<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"GEOADD" => cmd_geoadd(store, args, out),
        b"GEOPOS" => cmd_geopos(store, args, out),
        b"GEODIST" => cmd_geodist(store, args, out),
        b"GEOHASH" => cmd_geohash(store, args, out),
        b"GEOSEARCH" => search::cmd_geosearch(store, args, out),
        _ => return false,
    }
    true
}

/// Shared between GEODIST and GEOSEARCH. Returns the metres-per-unit
/// multiplier for `m | km | mi | ft`; `None` for unknown units.
pub(super) fn parse_unit(b: &[u8]) -> Option<f64> {
    match b.to_ascii_lowercase().as_slice() {
        b"m" => Some(1.0),
        b"km" => Some(1000.0),
        b"mi" => Some(1609.34),
        b"ft" => Some(0.3048),
        _ => None,
    }
}

// ───────────── GEOADD ─────────────

/// `GEOADD key [NX|XX] [CH] longitude latitude member [longitude latitude member ...]`
fn cmd_geoadd<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 5 {
        return wrong_args(out, "geoadd");
    }
    let (mode, ch, first_triple) = match parse_geoadd_flags(args) {
        Ok(t) => t,
        Err(msg) => return encode_error(out, msg),
    };
    if !(args.len() - first_triple).is_multiple_of(3) {
        return encode_error(out, "ERR syntax error");
    }
    let mut pairs: Vec<(f64, Vec<u8>)> = Vec::with_capacity((args.len() - first_triple) / 3);
    let mut i = first_triple;
    while i < args.len() {
        let lon = match arg_f64(&args[i]) {
            Some(v) => v,
            None => return encode_error(out, "ERR value is not a valid float"),
        };
        let lat = match arg_f64(&args[i + 1]) {
            Some(v) => v,
            None => return encode_error(out, "ERR value is not a valid float"),
        };
        let member = args[i + 2].to_vec();
        let score = match encode_score(lon, lat) {
            Some(s) => s,
            None => {
                return encode_error(
                    out,
                    &format!(
                        "ERR invalid longitude,latitude pair {lon:.6},{lat:.6}",
                    ),
                );
            }
        };
        pairs.push((score, member));
        i += 3;
    }
    let n = match apply_geoadd(store, &args[1], &pairs, mode, ch) {
        Ok(n) => n,
        Err(e) => return store_err(out, e),
    };
    kevy_resp::encode_integer(out, n as i64);
}

#[derive(Clone, Copy)]
enum GeoAddMode {
    Default,
    Nx,
    Xx,
}

/// Parse the optional `[NX|XX] [CH]` flags. Returns `(mode, ch, index of
/// first lon/lat/member triple)`. NX and XX are mutually exclusive
/// (matches Redis).
fn parse_geoadd_flags<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<(GeoAddMode, bool, usize), &'static str> {
    let mut mode = GeoAddMode::Default;
    let mut ch = false;
    let mut i = 2;
    while i < args.len() {
        let u = args[i].to_ascii_uppercase();
        match u.as_slice() {
            b"NX" => {
                if matches!(mode, GeoAddMode::Xx) {
                    return Err("ERR XX and NX options at the same time are not compatible");
                }
                mode = GeoAddMode::Nx;
                i += 1;
            }
            b"XX" => {
                if matches!(mode, GeoAddMode::Nx) {
                    return Err("ERR XX and NX options at the same time are not compatible");
                }
                mode = GeoAddMode::Xx;
                i += 1;
            }
            b"CH" => {
                ch = true;
                i += 1;
            }
            _ => break,
        }
    }
    Ok((mode, ch, i))
}

fn apply_geoadd(
    store: &mut Store,
    key: &[u8],
    pairs: &[(f64, Vec<u8>)],
    mode: GeoAddMode,
    ch: bool,
) -> Result<usize, kevy_store::StoreError> {
    let existing: Vec<Option<f64>> = pairs
        .iter()
        .map(|(_, m)| store.zscore(key, m).unwrap_or(Some(0.0)))
        .collect();
    let mut to_write: Vec<(f64, Vec<u8>)> = Vec::with_capacity(pairs.len());
    for (i, p) in pairs.iter().enumerate() {
        let exists = existing[i].is_some();
        let allowed = matches!(
            (mode, exists),
            (GeoAddMode::Default, _) | (GeoAddMode::Nx, false) | (GeoAddMode::Xx, true),
        );
        if allowed {
            to_write.push(p.clone());
        }
    }
    if to_write.is_empty() {
        return Ok(0);
    }
    let added = store.zadd(key, &to_write)?;
    if !ch {
        return Ok(added);
    }
    // CH = "changed": added + modified. We compute "modified" as
    // pre-write score ≠ post-write score for the members that existed.
    let changed = to_write
        .iter()
        .filter(|(s, m)| {
            let i = pairs.iter().position(|(_, mm)| mm == m).unwrap();
            existing[i].is_some_and(|old| old != *s)
        })
        .count();
    Ok(added + changed)
}

// ───────────── GEOPOS ─────────────

/// `GEOPOS key member [member ...]` — returns `[lon, lat]` pairs (or nil
/// for missing members). Coordinates are rendered with 17-digit
/// precision to match Redis's `addReplyHumanLongDouble`.
fn cmd_geopos<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "geopos");
    }
    let n = args.len() - 2;
    encode_array_len(out, n as i64);
    for i in 0..n {
        match store.zscore(&args[1], &args[i + 2]) {
            Ok(Some(score)) => {
                let (lon, lat) = decode_score(score);
                encode_array_len(out, 2);
                encode_bulk(out, fmt_geo_coord(lon).as_bytes());
                encode_bulk(out, fmt_geo_coord(lat).as_bytes());
            }
            // GEOPOS returns the null array (`*-1\r\n`) for missing
            // members — matches Redis exactly.
            Ok(None) => encode_array_len(out, -1),
            Err(e) => return store_err(out, e),
        }
    }
}

/// Match Redis's GEO coordinate string format — fixed 17-digit precision
/// after the decimal point so client tools that parse the textual reply
/// see the full f64 mantissa we encoded into the score.
fn fmt_geo_coord(v: f64) -> String {
    format!("{v:.17}")
}

// ───────────── GEODIST ─────────────

/// `GEODIST key member1 member2 [m|km|mi|ft]`
fn cmd_geodist<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if !(4..=5).contains(&args.len()) {
        return wrong_args(out, "geodist");
    }
    let unit = if args.len() == 5 {
        match parse_unit(&args[4]) {
            Some(u) => u,
            None => {
                return encode_error(out, "ERR unsupported unit provided. please use M, KM, FT, MI");
            }
        }
    } else {
        1.0
    };
    let p1 = match score_to_point(store, &args[1], &args[2]) {
        Ok(Some(p)) => p,
        Ok(None) => return encode_null_bulk(out),
        Err(e) => return store_err(out, e),
    };
    let p2 = match score_to_point(store, &args[1], &args[3]) {
        Ok(Some(p)) => p,
        Ok(None) => return encode_null_bulk(out),
        Err(e) => return store_err(out, e),
    };
    let d = haversine_meters(p1.0, p1.1, p2.0, p2.1) / unit;
    encode_bulk(out, format!("{d:.4}").as_bytes());
}

fn score_to_point(
    store: &mut Store,
    key: &[u8],
    member: &[u8],
) -> Result<Option<(f64, f64)>, kevy_store::StoreError> {
    Ok(store.zscore(key, member)?.map(decode_score))
}

// ───────────── GEOHASH ─────────────

/// `GEOHASH key member [member ...]` — emits the 11-character base32
/// geohash for each member's cell centre (nil for missing members).
fn cmd_geohash<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "geohash");
    }
    let n = args.len() - 2;
    encode_array_len(out, n as i64);
    for i in 0..n {
        match store.zscore(&args[1], &args[i + 2]) {
            Ok(Some(score)) => {
                let (lon_c, lat_c) = decode_score(score);
                let buf = encode_base32_geohash(lon_c, lat_c);
                encode_bulk(out, &buf);
            }
            Ok(None) => encode_null_bulk(out),
            Err(e) => return store_err(out, e),
        }
    }
}
