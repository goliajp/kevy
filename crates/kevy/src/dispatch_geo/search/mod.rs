//! `GEOSEARCH` / `GEOSEARCHSTORE` — query members within a radius or
//! bounding box of an anchor point and (optionally) write the result
//! into a destination ZSet. Also hosts the type / helper layer shared
//! with the legacy `GEORADIUS[BYMEMBER]` family in `radius.rs`.
//!
//! Sub-modules:
//! - `parse` — argv-soup → structured `Opts` for GEOSEARCH /
//!   GEOSEARCHSTORE / legacy GEORADIUS. Kept separate so this file
//!   stays under the project's ≤500-LOC limit.

mod parse;

pub(super) use parse::{parse_legacy_radius, parse_opts, parse_opts_at};

use kevy_geo::{
    EARTH_RADIUS_METERS, decode_score, haversine_meters, neighbor_score_ranges,
};
use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer,
};
use kevy_store::{ScoreBound, Store};

use crate::cmd::{store_err, wrong_args};

use super::score_to_point;

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
    let hits = match run_search(store, &key, &opts) {
        Ok(h) => h,
        Err(SearchError::NoMember) => {
            return encode_error(out, "ERR could not decode requested zset member");
        }
        Err(SearchError::Store(e)) => return store_err(out, e),
    };
    emit_reply(&hits, &opts, out);
}

/// Shared search core: resolves the centre, fans out over the candidate
/// neighbour ranges, filters by exact shape, then applies sort + count.
/// Used directly by `GEOSEARCH` for its reply path and indirectly by
/// `GEOSEARCHSTORE` / `GEORADIUS*` (sprint C) for theirs.
pub(super) fn run_search(
    store: &mut Store,
    key: &[u8],
    opts: &Opts,
) -> Result<Vec<Hit>, SearchError> {
    let (clon, clat) = resolve_center(store, key, &opts.from)?;
    let ranges = neighbor_score_ranges(clon, clat, opts.shape.bounding_radius_meters());
    let mut hits = collect_hits(store, key, &ranges, clon, clat, opts)?;
    apply_sort(&mut hits, opts.sort);
    apply_count(&mut hits, opts.count, opts.any);
    Ok(hits)
}

pub(super) enum SearchError {
    NoMember,
    Store(kevy_store::StoreError),
}

impl From<kevy_store::StoreError> for SearchError {
    fn from(e: kevy_store::StoreError) -> Self {
        SearchError::Store(e)
    }
}

// ───────────── options ─────────────

pub(super) enum Anchor {
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

#[derive(Default, Clone, Copy)]
enum Sort {
    #[default]
    None,
    Asc,
    Desc,
}

pub(super) struct Opts {
    from: Anchor,
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
    /// `STOREDIST` flag (GEOSEARCHSTORE / GEORADIUS only): write the
    /// metric distance to dst as the ZSet score instead of the
    /// geohash. GEOSEARCH ignores this field.
    pub(super) storedist: bool,
}


// ───────────── candidate collection ─────────────

pub(super) struct Hit {
    pub(super) member: Vec<u8>,
    pub(super) score: f64,
    pub(super) dist_m: f64,
}

fn resolve_center(
    store: &mut Store,
    key: &[u8],
    from: &Anchor,
) -> Result<(f64, f64), SearchError> {
    match from {
        Anchor::Member(m) => match score_to_point(store, key, m) {
            Ok(Some(p)) => Ok(p),
            Ok(None) => Err(SearchError::NoMember),
            Err(e) => Err(SearchError::Store(e)),
        },
        Anchor::LonLat(lon, lat) => Ok((*lon, *lat)),
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

// ───────────── legacy GEORADIUS option parsing ─────────────

/// Parsed form of a `GEORADIUS[BYMEMBER]` invocation: search-core
/// `Opts` plus the optional STORE destination it should write into
/// instead of replying.
pub(super) struct LegacyRadiusParsed {
    pub(super) opts: Opts,
    pub(super) store_dst: Option<Vec<u8>>,
}

/// What `emit_or_store` did with the hits: emitted them as a wire
/// reply already, or wrote them into a destination ZSet (returning
/// the integer count to be encoded by the caller).
pub(super) enum RadiusReply {
    Replied,
    Stored(Result<usize, kevy_store::StoreError>),
}

pub(super) fn emit_or_store(
    out: &mut Vec<u8>,
    store: &mut Store,
    hits: &[Hit],
    parsed: &LegacyRadiusParsed,
) -> RadiusReply {
    match &parsed.store_dst {
        None => {
            emit_reply(hits, &parsed.opts, out);
            RadiusReply::Replied
        }
        Some(dst) => RadiusReply::Stored(write_hits_to_zset(store, dst, hits, parsed.opts.storedist)),
    }
}

// ───────────── GEOSEARCHSTORE ─────────────

/// `GEOSEARCHSTORE destination source <FROMMEMBER|FROMLONLAT...>
/// <BYRADIUS|BYBOX...> [ASC|DESC] [COUNT n [ANY]] [STOREDIST]`
///
/// Runs the same search core, then writes the hits into `destination`
/// as a ZSet whose score is either the source geohash (default) or
/// the metric distance (when `STOREDIST` is set). Pre-existing
/// destination contents are dropped — matches Redis exactly. Reply is
/// the integer count of stored members.
pub(super) fn cmd_geosearchstore<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() < 5 {
        return wrong_args(out, "geosearchstore");
    }
    let opts = match parse_opts_at(args, 3) {
        Ok(o) => o,
        Err(msg) => return encode_error(out, msg),
    };
    let dst = args[1].to_vec();
    let src = args[2].to_vec();
    let hits = match run_search(store, &src, &opts) {
        Ok(h) => h,
        Err(SearchError::NoMember) => {
            return encode_error(out, "ERR could not decode requested zset member");
        }
        Err(SearchError::Store(e)) => return store_err(out, e),
    };
    match write_hits_to_zset(store, &dst, &hits, opts.storedist) {
        Ok(n) => encode_integer(out, n as i64),
        Err(e) => store_err(out, e),
    }
}

/// Atomically replace `dst` with a ZSet built from `hits`. `storedist`
/// controls whether the score is the source geohash (`false`) or the
/// metric distance in metres (`true`). An empty `hits` slice deletes
/// `dst`, matching Redis's "no key on empty result" behaviour.
fn write_hits_to_zset(
    store: &mut Store,
    dst: &[u8],
    hits: &[Hit],
    storedist: bool,
) -> Result<usize, kevy_store::StoreError> {
    store.del(&[dst.to_vec()]);
    if hits.is_empty() {
        return Ok(0);
    }
    let pairs: Vec<(f64, Vec<u8>)> = hits
        .iter()
        .map(|h| {
            let score = if storedist { h.dist_m } else { h.score };
            (score, h.member.clone())
        })
        .collect();
    store.zadd(dst, &pairs)?;
    Ok(pairs.len())
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
