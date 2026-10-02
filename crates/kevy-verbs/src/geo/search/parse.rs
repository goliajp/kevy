//! Option parsing for `GEOSEARCH` / `GEOSEARCHSTORE` and the legacy
//! `GEORADIUS` family. Kept in its own file so `search/mod.rs` (which
//! owns the search core + reply emission + STORE write path) stays
//! under the project's 500-LOC limit.

use kevy_resp::ArgvView;
use kevy_resp::{CmdError, RespVersion, encode_error};

use crate::args::arg_f64;

use super::super::parse_unit;
use super::{Anchor, LegacyRadiusParsed, Opts, Shape, Sort};

/// Why a geo search's arguments were refused: a fixed reply, or a centre
/// off the map, whose reply quotes the pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::geo) enum GeoError {
    Wire(&'static str),
    BadCenter(f64, f64),
}

impl From<&'static str> for GeoError {
    fn from(s: &'static str) -> Self {
        Self::Wire(s)
    }
}

impl From<CmdError> for GeoError {
    fn from(e: CmdError) -> Self {
        Self::Wire(e.as_wire())
    }
}

impl GeoError {
    pub(in crate::geo) fn emit(self, out: &mut Vec<u8>) {
        match self {
            Self::Wire(s) => encode_error(out, s),
            Self::BadCenter(lon, lat) => encode_error(out, &bad_pair(lon, lat)),
        }
    }
}

/// Redis's reply to a longitude / latitude pair off the map.
pub(in crate::geo) fn bad_pair(lon: f64, lat: f64) -> String {
    format!("ERR invalid longitude,latitude pair {lon:.6},{lat:.6}")
}

/// A centre's longitude and latitude, checked against the map.
pub(in crate::geo) fn center(lon: &[u8], lat: &[u8]) -> Result<Anchor, GeoError> {
    let lon = arg_f64(lon).ok_or("ERR value is not a valid float")?;
    let lat = arg_f64(lat).ok_or("ERR value is not a valid float")?;
    if kevy_geo::encode_score(lon, lat).is_none() {
        return Err(GeoError::BadCenter(lon, lat));
    }
    Ok(Anchor::LonLat(lon, lat))
}

/// A radius, a width or a height: a number, and not negative.
pub(in crate::geo) fn extent(b: &[u8], unparsed: &'static str) -> Result<f64, GeoError> {
    arg_f64(b).ok_or(GeoError::Wire(unparsed))
}

pub(in crate::geo) fn parse_opts<A: ArgvView + ?Sized>(args: &A) -> Result<Opts, GeoError> {
    parse_opts_at(args, 2)
}

/// Same as [`parse_opts`] but starts scanning at `start` instead of `2`.
/// `GEOSEARCHSTORE` uses `start=3` (verb, dst, src); GEOSEARCH uses 2.
pub(in crate::geo) fn parse_opts_at<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
) -> Result<Opts, GeoError> {
    let mut state = OptsBuilder::default();
    let mut i = start;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        i += parse_one_opt(args, &tok, i, &mut state)?;
    }
    state.finish()
}

/// Translate a `GEORADIUS[BYMEMBER]` argv (legacy: fixed prefix then
/// flag soup, `STORE key` / `STOREDIST key` recognised as positional
/// dst keys) into the structured `Opts` the search core expects.
pub(in crate::geo) fn parse_legacy_radius<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    anchor: Anchor,
    radius_m: f64,
    unit: f64,
) -> Result<LegacyRadiusParsed, GeoError> {
    let mut s = OptsBuilder {
        from: Some(anchor),
        shape: Some((Shape::Radius { r_m: radius_m }, unit)),
        ..OptsBuilder::default()
    };
    let mut store_dst: Option<Vec<u8>> = None;
    let mut i = start;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        i += match tok.as_slice() {
            b"STORE" => {
                let dst = args.get(i + 1).ok_or("ERR syntax error")?;
                store_dst = Some(dst.to_vec());
                s.storedist = false;
                2
            }
            b"STOREDIST" => {
                let dst = args.get(i + 1).ok_or("ERR syntax error")?;
                store_dst = Some(dst.to_vec());
                s.storedist = true;
                2
            }
            _ => parse_one_opt(args, &tok, i, &mut s)?,
        };
    }
    if store_dst.is_some() && (s.with_coord || s.with_dist || s.with_hash) {
        return Err(GeoError::Wire(
            "ERR STORE option in GEORADIUS is not compatible with WITHCOORD, WITHDIST and WITHHASH options",
        ));
    }
    let opts = s.finish()?;
    Ok(LegacyRadiusParsed { opts, store_dst })
}

#[derive(Default)]
pub(super) struct OptsBuilder {
    pub(super) from: Option<Anchor>,
    pub(super) shape: Option<(Shape, f64)>,
    pub(super) sort: Sort,
    pub(super) count: Option<usize>,
    pub(super) any: bool,
    pub(super) with_coord: bool,
    pub(super) with_dist: bool,
    pub(super) with_hash: bool,
    pub(super) storedist: bool,
}

impl OptsBuilder {
    fn finish(self) -> Result<Opts, GeoError> {
        let from = self.from.ok_or("ERR syntax error: missing FROMMEMBER / FROMLONLAT")?;
        let (shape, unit) = self.shape.ok_or("ERR syntax error: missing BYRADIUS / BYBOX")?;
        Ok(Opts {
            from,
            shape,
            unit,
            sort: self.sort,
            count: self.count,
            any: self.any,
            with_coord: self.with_coord,
            with_dist: self.with_dist,
            with_hash: self.with_hash,
            storedist: self.storedist,
            proto: RespVersion::V2,
        })
    }
}

/// Consume one option starting at `args[i]`. Returns how many args
/// were consumed (1..=4). Mutates the partial-Opts state in place.
fn parse_one_opt<A: ArgvView + ?Sized>(
    args: &A,
    tok: &[u8],
    i: usize,
    s: &mut OptsBuilder,
) -> Result<usize, GeoError> {
    match tok {
        b"FROMMEMBER" | b"FROMLONLAT" => parse_from(args, tok, i, &mut s.from),
        b"BYRADIUS" | b"BYBOX" => parse_shape(args, tok, i, &mut s.shape),
        b"ASC" => {
            s.sort = Sort::Asc;
            Ok(1)
        }
        b"DESC" => {
            s.sort = Sort::Desc;
            Ok(1)
        }
        b"COUNT" => parse_count(args, i, &mut s.count, &mut s.any),
        b"WITHCOORD" => {
            s.with_coord = true;
            Ok(1)
        }
        b"WITHDIST" => {
            s.with_dist = true;
            Ok(1)
        }
        b"WITHHASH" => {
            s.with_hash = true;
            Ok(1)
        }
        b"STOREDIST" => {
            s.storedist = true;
            Ok(1)
        }
        _ => Err(GeoError::Wire("ERR syntax error")),
    }
}

fn parse_from<A: ArgvView + ?Sized>(
    args: &A,
    tok: &[u8],
    i: usize,
    from: &mut Option<Anchor>,
) -> Result<usize, GeoError> {
    if tok == b"FROMMEMBER" {
        let m = args.get(i + 1).ok_or("ERR syntax error")?;
        *from = Some(Anchor::Member(m.to_vec()));
        return Ok(2);
    }
    let lon = args.get(i + 1).ok_or("ERR syntax error")?;
    let lat = args.get(i + 2).ok_or("ERR syntax error")?;
    *from = Some(center(lon, lat)?);
    Ok(3)
}

fn parse_shape<A: ArgvView + ?Sized>(
    args: &A,
    tok: &[u8],
    i: usize,
    shape: &mut Option<(Shape, f64)>,
) -> Result<usize, GeoError> {
    if tok == b"BYRADIUS" {
        let r = extent(args.get(i + 1).ok_or("ERR syntax error")?, "ERR need numeric radius")?;
        if r < 0.0 {
            return Err(GeoError::Wire("ERR radius cannot be negative"));
        }
        let u = parse_unit(args.get(i + 2).ok_or("ERR syntax error")?)
            .ok_or("ERR unsupported unit provided. please use M, KM, FT, MI")?;
        *shape = Some((Shape::Radius { r_m: r * u }, u));
        return Ok(3);
    }
    let w = extent(args.get(i + 1).ok_or("ERR syntax error")?, "ERR need numeric width")?;
    let h = extent(args.get(i + 2).ok_or("ERR syntax error")?, "ERR need numeric height")?;
    if w < 0.0 || h < 0.0 {
        return Err(GeoError::Wire("ERR height or width cannot be negative"));
    }
    let u = parse_unit(args.get(i + 3).ok_or("ERR syntax error")?)
        .ok_or("ERR unsupported unit provided. please use M, KM, FT, MI")?;
    *shape = Some((Shape::Box { w_m: w * u, h_m: h * u }, u));
    Ok(4)
}

fn parse_count<A: ArgvView + ?Sized>(
    args: &A,
    i: usize,
    count: &mut Option<usize>,
    any: &mut bool,
) -> Result<usize, GeoError> {
    let n: i64 = std::str::from_utf8(args.get(i + 1).ok_or("ERR syntax error")?)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("ERR value is not an integer or out of range")?;
    if n <= 0 {
        return Err(GeoError::Wire("ERR COUNT can't be negative"));
    }
    *count = Some(n as usize);
    if let Some(next) = args.get(i + 2)
        && next.eq_ignore_ascii_case(b"ANY")
    {
        *any = true;
        return Ok(3);
    }
    Ok(2)
}
