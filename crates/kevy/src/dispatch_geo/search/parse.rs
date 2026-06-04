//! Option parsing for `GEOSEARCH` / `GEOSEARCHSTORE` and the legacy
//! `GEORADIUS` family. Kept in its own file so `search/mod.rs` (which
//! owns the search core + reply emission + STORE write path) stays
//! under the project's 500-LOC limit.

use kevy_resp::ArgvView;

use crate::cmd::arg_f64;

use super::super::parse_unit;
use super::{Anchor, LegacyRadiusParsed, Opts, Shape, Sort};

pub(in crate::dispatch_geo) fn parse_opts<A: ArgvView + ?Sized>(args: &A) -> Result<Opts, &'static str> {
    parse_opts_at(args, 2)
}

/// Same as [`parse_opts`] but starts scanning at `start` instead of `2`.
/// `GEOSEARCHSTORE` uses `start=3` (verb, dst, src); GEOSEARCH uses 2.
pub(in crate::dispatch_geo) fn parse_opts_at<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
) -> Result<Opts, &'static str> {
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
pub(in crate::dispatch_geo) fn parse_legacy_radius<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    anchor: Anchor,
    radius_m: f64,
    unit: f64,
) -> Result<LegacyRadiusParsed, &'static str> {
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
        return Err(
            "ERR STORE option in GEORADIUS is not compatible with WITHCOORD, WITHDIST and WITHHASH options",
        );
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
    fn finish(self) -> Result<Opts, &'static str> {
        let from = self
            .from
            .ok_or("ERR syntax error: missing FROMMEMBER / FROMLONLAT")?;
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
) -> Result<usize, &'static str> {
    match tok {
        b"FROMMEMBER" | b"FROMLONLAT" => parse_from(args, tok, i, &mut s.from),
        b"BYRADIUS" | b"BYBOX" => parse_shape(args, tok, i, &mut s.shape),
        b"ASC" => { s.sort = Sort::Asc; Ok(1) }
        b"DESC" => { s.sort = Sort::Desc; Ok(1) }
        b"COUNT" => parse_count(args, i, &mut s.count, &mut s.any),
        b"WITHCOORD" => { s.with_coord = true; Ok(1) }
        b"WITHDIST" => { s.with_dist = true; Ok(1) }
        b"WITHHASH" => { s.with_hash = true; Ok(1) }
        b"STOREDIST" => { s.storedist = true; Ok(1) }
        _ => Err("ERR syntax error"),
    }
}

fn parse_from<A: ArgvView + ?Sized>(
    args: &A,
    tok: &[u8],
    i: usize,
    from: &mut Option<Anchor>,
) -> Result<usize, &'static str> {
    if tok == b"FROMMEMBER" {
        let m = args.get(i + 1).ok_or("ERR syntax error")?;
        *from = Some(Anchor::Member(m.to_vec()));
        return Ok(2);
    }
    let lon = arg_f64(args.get(i + 1).ok_or("ERR syntax error")?)
        .ok_or("ERR value is not a valid float")?;
    let lat = arg_f64(args.get(i + 2).ok_or("ERR syntax error")?)
        .ok_or("ERR value is not a valid float")?;
    *from = Some(Anchor::LonLat(lon, lat));
    Ok(3)
}

fn parse_shape<A: ArgvView + ?Sized>(
    args: &A,
    tok: &[u8],
    i: usize,
    shape: &mut Option<(Shape, f64)>,
) -> Result<usize, &'static str> {
    if tok == b"BYRADIUS" {
        let r = arg_f64(args.get(i + 1).ok_or("ERR syntax error")?)
            .ok_or("ERR value is not a valid float")?;
        let u = parse_unit(args.get(i + 2).ok_or("ERR syntax error")?)
            .ok_or("ERR unsupported unit provided. please use M, KM, FT, MI")?;
        *shape = Some((Shape::Radius { r_m: r * u }, u));
        return Ok(3);
    }
    let w = arg_f64(args.get(i + 1).ok_or("ERR syntax error")?)
        .ok_or("ERR value is not a valid float")?;
    let h = arg_f64(args.get(i + 2).ok_or("ERR syntax error")?)
        .ok_or("ERR value is not a valid float")?;
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
) -> Result<usize, &'static str> {
    let n: i64 = std::str::from_utf8(args.get(i + 1).ok_or("ERR syntax error")?)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("ERR value is not an integer or out of range")?;
    if n <= 0 {
        return Err("ERR COUNT can't be negative");
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
