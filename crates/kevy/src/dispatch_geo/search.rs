//! `GEOSEARCH` — query members within a radius or bounding box of an
//! anchor point (a literal `(lon, lat)` or another member of the same
//! key). The full surface area of the command — radius / box geometries,
//! ASC/DESC + COUNT + ANY trimming, three optional reply enrichments
//! (`WITHCOORD` / `WITHDIST` / `WITHHASH`) — lives here so the parent
//! module stays under the project's ≤500-LOC limit.

use kevy_geo::{
    EARTH_RADIUS_METERS, decode_score, haversine_meters, neighbor_score_ranges,
};
use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer,
};
use kevy_store::{ScoreBound, Store};

use crate::cmd::{arg_f64, store_err, wrong_args};

use super::{parse_unit, score_to_point};

/// `GEOSEARCH key <FROMMEMBER member|FROMLONLAT lon lat>
/// <BYRADIUS r unit|BYBOX w h unit> [ASC|DESC] [COUNT n [ANY]]
/// [WITHCOORD] [WITHDIST] [WITHHASH]`
pub(super) fn cmd_geosearch<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() < 4 {
        return wrong_args(out, "geosearch");
    }
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(msg) => return encode_error(out, msg),
    };
    let key = args[1].to_vec();
    let (clon, clat) = match resolve_center(store, &key, &opts.from) {
        Ok(c) => c,
        Err(GeoCenterError::NoMember) => {
            return encode_error(out, "ERR could not decode requested zset member");
        }
        Err(GeoCenterError::Store(e)) => return store_err(out, e),
    };
    let bound_radius = opts.shape.bounding_radius_meters();
    let ranges = neighbor_score_ranges(clon, clat, bound_radius);
    let mut hits = match collect_hits(store, &key, &ranges, clon, clat, &opts) {
        Ok(h) => h,
        Err(e) => return store_err(out, e),
    };
    apply_sort(&mut hits, opts.sort);
    apply_count(&mut hits, opts.count, opts.any);
    emit_reply(&hits, &opts, out);
}

// ───────────── options ─────────────

enum From {
    Member(Vec<u8>),
    LonLat(f64, f64),
}

#[derive(Clone, Copy)]
enum Shape {
    Radius { r_m: f64 },
    Box { w_m: f64, h_m: f64 },
}

impl Shape {
    /// Bound the shape with a disc of this radius (used as the radius
    /// passed to `neighbor_score_ranges` for candidate pruning). For a
    /// box, the radius of the circumscribing circle is `sqrt(w² + h²)/2`.
    fn bounding_radius_meters(&self) -> f64 {
        match *self {
            Shape::Radius { r_m } => r_m,
            Shape::Box { w_m, h_m } => 0.5 * (w_m * w_m + h_m * h_m).sqrt(),
        }
    }
}

#[derive(Clone, Copy)]
enum Sort {
    None,
    Asc,
    Desc,
}

struct Opts {
    from: From,
    shape: Shape,
    /// Unit multiplier (metres per unit) for the `BYRADIUS r unit` /
    /// `BYBOX w h unit` argument; reapplied when formatting `WITHDIST`.
    unit: f64,
    sort: Sort,
    count: Option<usize>,
    any: bool,
    with_coord: bool,
    with_dist: bool,
    with_hash: bool,
}

fn parse_opts<A: ArgvView + ?Sized>(args: &A) -> Result<Opts, &'static str> {
    let mut from: Option<From> = None;
    let mut shape: Option<(Shape, f64)> = None;
    let mut sort = Sort::None;
    let mut count: Option<usize> = None;
    let mut any = false;
    let mut with_coord = false;
    let mut with_dist = false;
    let mut with_hash = false;
    let mut i = 2;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        i += parse_one_opt(
            args, &tok, i, &mut from, &mut shape, &mut sort, &mut count, &mut any,
            &mut with_coord, &mut with_dist, &mut with_hash,
        )?;
    }
    let from = from.ok_or("ERR syntax error: missing FROMMEMBER / FROMLONLAT")?;
    let (shape, unit) = shape.ok_or("ERR syntax error: missing BYRADIUS / BYBOX")?;
    Ok(Opts {
        from, shape, unit, sort, count, any, with_coord, with_dist, with_hash,
    })
}

/// Consume one option starting at `args[i]`. Returns how many args
/// were consumed (1..=4). Mutates the partial-Opts fields in place.
#[allow(clippy::too_many_arguments)]
fn parse_one_opt<A: ArgvView + ?Sized>(
    args: &A,
    tok: &[u8],
    i: usize,
    from: &mut Option<From>,
    shape: &mut Option<(Shape, f64)>,
    sort: &mut Sort,
    count: &mut Option<usize>,
    any: &mut bool,
    with_coord: &mut bool,
    with_dist: &mut bool,
    with_hash: &mut bool,
) -> Result<usize, &'static str> {
    match tok {
        b"FROMMEMBER" | b"FROMLONLAT" => parse_from(args, tok, i, from),
        b"BYRADIUS" | b"BYBOX" => parse_shape(args, tok, i, shape),
        b"ASC" => { *sort = Sort::Asc; Ok(1) }
        b"DESC" => { *sort = Sort::Desc; Ok(1) }
        b"COUNT" => parse_count(args, i, count, any),
        b"WITHCOORD" => { *with_coord = true; Ok(1) }
        b"WITHDIST" => { *with_dist = true; Ok(1) }
        b"WITHHASH" => { *with_hash = true; Ok(1) }
        _ => Err("ERR syntax error"),
    }
}

fn parse_from<A: ArgvView + ?Sized>(
    args: &A,
    tok: &[u8],
    i: usize,
    from: &mut Option<From>,
) -> Result<usize, &'static str> {
    if tok == b"FROMMEMBER" {
        let m = args.get(i + 1).ok_or("ERR syntax error")?;
        *from = Some(From::Member(m.to_vec()));
        return Ok(2);
    }
    let lon = arg_f64(args.get(i + 1).ok_or("ERR syntax error")?)
        .ok_or("ERR value is not a valid float")?;
    let lat = arg_f64(args.get(i + 2).ok_or("ERR syntax error")?)
        .ok_or("ERR value is not a valid float")?;
    *from = Some(From::LonLat(lon, lat));
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

// ───────────── candidate collection ─────────────

struct Hit {
    member: Vec<u8>,
    score: f64,
    dist_m: f64,
}

enum GeoCenterError {
    NoMember,
    Store(kevy_store::StoreError),
}

fn resolve_center(
    store: &mut Store,
    key: &[u8],
    from: &From,
) -> Result<(f64, f64), GeoCenterError> {
    match from {
        From::Member(m) => match score_to_point(store, key, m) {
            Ok(Some(p)) => Ok(p),
            Ok(None) => Err(GeoCenterError::NoMember),
            Err(e) => Err(GeoCenterError::Store(e)),
        },
        From::LonLat(lon, lat) => Ok((*lon, *lat)),
    }
}

fn collect_hits(
    store: &mut Store,
    key: &[u8],
    ranges: &[(f64, f64)],
    clon: f64,
    clat: f64,
    opts: &Opts,
) -> Result<Vec<Hit>, kevy_store::StoreError> {
    let mut hits = Vec::new();
    for (min, max) in ranges {
        let members = store.zrange_by_score(
            key,
            ScoreBound { value: *min, exclusive: false },
            ScoreBound { value: *max, exclusive: false },
        )?;
        for (member, score) in members {
            let (mlon, mlat) = decode_score(score);
            if !in_shape(opts.shape, clon, clat, mlon, mlat) {
                continue;
            }
            let dist_m = haversine_meters(clon, clat, mlon, mlat);
            hits.push(Hit { member, score, dist_m });
        }
    }
    Ok(hits)
}

fn in_shape(shape: Shape, clon: f64, clat: f64, mlon: f64, mlat: f64) -> bool {
    match shape {
        Shape::Radius { r_m } => haversine_meters(clon, clat, mlon, mlat) <= r_m,
        Shape::Box { w_m, h_m } => {
            // On-ground rectangle: project ∆lat/∆lon to metres and
            // compare against half-axes. The lon component shrinks by
            // cos(lat) at higher latitudes (the standard small-box
            // approximation Redis uses).
            let dlat_m = (mlat - clat).to_radians() * EARTH_RADIUS_METERS;
            let dlon_m = (mlon - clon).to_radians()
                * EARTH_RADIUS_METERS
                * clat.to_radians().cos();
            dlat_m.abs() <= h_m / 2.0 && dlon_m.abs() <= w_m / 2.0
        }
    }
}

fn apply_sort(hits: &mut [Hit], sort: Sort) {
    match sort {
        Sort::Asc => hits.sort_by(|a, b| a.dist_m.partial_cmp(&b.dist_m).unwrap()),
        Sort::Desc => hits.sort_by(|a, b| b.dist_m.partial_cmp(&a.dist_m).unwrap()),
        Sort::None => {}
    }
}

fn apply_count(hits: &mut Vec<Hit>, count: Option<usize>, any: bool) {
    if let Some(n) = count {
        // Without ANY, COUNT pairs with an implicit ASC sort so the
        // closest N are returned. With ANY, the slice order is left
        // as-collected for the speed-vs-determinism trade-off.
        if !any && !is_sorted_asc(hits) {
            hits.sort_by(|a, b| a.dist_m.partial_cmp(&b.dist_m).unwrap());
        }
        hits.truncate(n);
    }
}

fn is_sorted_asc(hits: &[Hit]) -> bool {
    hits.windows(2).all(|w| w[0].dist_m <= w[1].dist_m)
}

// ───────────── reply ─────────────

fn emit_reply(hits: &[Hit], opts: &Opts, out: &mut Vec<u8>) {
    let any_with = opts.with_coord || opts.with_dist || opts.with_hash;
    encode_array_len(out, hits.len() as i64);
    if !any_with {
        for h in hits {
            encode_bulk(out, &h.member);
        }
        return;
    }
    for h in hits {
        let extras = opts.with_dist as i64
            + opts.with_hash as i64
            + opts.with_coord as i64;
        encode_array_len(out, 1 + extras);
        encode_bulk(out, &h.member);
        if opts.with_dist {
            encode_bulk(out, format!("{:.4}", h.dist_m / opts.unit).as_bytes());
        }
        if opts.with_hash {
            encode_integer(out, h.score as i64);
        }
        if opts.with_coord {
            let (lon, lat) = decode_score(h.score);
            encode_array_len(out, 2);
            encode_bulk(out, format!("{lon:.17}").as_bytes());
            encode_bulk(out, format!("{lat:.17}").as_bytes());
        }
    }
}
