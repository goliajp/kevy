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

use kevy_geo::{EARTH_RADIUS_METERS, decode_score, haversine_meters, neighbor_score_ranges};
use kevy_resp::{ArgvView, CmdError, encode_array_len, encode_bulk, encode_error, encode_integer};
use kevy_store::{ScoreBound, Store};

use crate::cmd::{store_err, wrong_args};

use super::score_to_point;

/// `GEOSEARCH key <FROMMEMBER member|FROMLONLAT lon lat>
/// <BYRADIUS r unit|BYBOX w h unit> [ASC|DESC] [COUNT n [ANY]]
/// [WITHCOORD] [WITHDIST] [WITHHASH]`
pub(super) fn cmd_geosearch<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 4 {
        return wrong_args(out, "geosearch");
    }
    let opts = match parse_opts(args) {
        Ok(o) => o,
        Err(msg) => return encode_error(out, msg.as_wire()),
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
    apply_count(&mut hits, opts.sort, opts.count, opts.any);
    Ok(hits)
}

/// The search half of a geo `*STORE`: the `(member, score)` pairs the
/// destination ZSet gets. Used by the single-shard dispatch path below and,
/// on a multi-shard server, by the runtime's `Op::GeoSearch` — which runs it
/// on the SOURCE's shard and ships these pairs to the DESTINATION's shard.
pub(super) fn search_pairs(
    store: &mut Store,
    key: &[u8],
    opts: &Opts,
) -> Result<Vec<(Vec<u8>, f64)>, SearchError> {
    let hits = run_search(store, key, opts)?;
    Ok(store_pairs(&hits, opts))
}

/// `STOREDIST` stores the distance **in the unit the query asked for** — a
/// `km` search stores 166.27, not 166274.15 (Redis's `geoAppendIfWithinShape`
/// divides by the shape's `conversion`). Without it, the score is the source
/// member's geohash, which is what makes a stored key a valid GEO key again.
fn store_pairs(hits: &[Hit], opts: &Opts) -> Vec<(Vec<u8>, f64)> {
    hits.iter()
        .map(|h| {
            let score = if opts.storedist { h.dist_m / opts.unit } else { h.score };
            (h.member.clone(), score)
        })
        .collect()
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

fn resolve_center(store: &mut Store, key: &[u8], from: &Anchor) -> Result<(f64, f64), SearchError> {
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
            let dlon_m = (mlon - clon).to_radians() * EARTH_RADIUS_METERS * clat.to_radians().cos();
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

fn apply_count(hits: &mut Vec<Hit>, sort: Sort, count: Option<usize>, any: bool) {
    let Some(n) = count else { return };
    // COUNT with no explicit ASC/DESC implies "the closest n" — Redis sorts
    // ascending before truncating. An explicit sort has already ordered the
    // hits (`apply_sort`), and truncating a DESC list keeps the FARTHEST n:
    // re-sorting ascending here returned the nearest n instead — the opposite
    // result set. ANY keeps the as-collected order (the documented
    // speed-vs-determinism trade).
    if matches!(sort, Sort::None) && !any {
        hits.sort_by(|a, b| a.dist_m.partial_cmp(&b.dist_m).unwrap());
    }
    hits.truncate(n);
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
    Stored(usize),
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
        // Single-shard path only: with the two keys on different shards the
        // runtime never gets here — it routes the write to `dst`'s shard.
        Some(dst) => {
            let pairs = store_pairs(hits, &parsed.opts);
            RadiusReply::Stored(store.zstore_result(dst, &pairs))
        }
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
    let (src, opts) = match plan_geosearchstore(args) {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    let dst = args[1].to_vec();
    match search_pairs(store, &src, &opts) {
        Ok(pairs) => encode_integer(out, store.zstore_result(&dst, &pairs) as i64),
        Err(SearchError::NoMember) => {
            encode_error(out, "ERR could not decode requested zset member");
        }
        Err(SearchError::Store(e)) => store_err(out, e),
    }
}

/// `GEOSEARCHSTORE dst src …` → `(source key, options)`. The destination is
/// argv[1] and belongs to the caller: on a multi-shard server the write lands
/// on ITS shard, not the source's (see `kevy_rt::Route::GeoStore`).
pub(super) fn plan_geosearchstore<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<(Vec<u8>, Opts), CmdError> {
    if args.len() < 5 {
        return Err(CmdError::Wire("ERR wrong number of arguments for 'geosearchstore' command"));
    }
    let opts = parse_opts_at(args, 3)?;
    Ok((args[2].to_vec(), opts))
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
        let extras =
            i64::from(opts.with_dist) + i64::from(opts.with_hash) + i64::from(opts.with_coord);
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
