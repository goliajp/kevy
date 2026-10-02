//! `GEOSEARCH` / `GEOSEARCHSTORE` — query members within a radius or
//! bounding box of an anchor point and (optionally) write the result
//! into a destination ZSet. Also hosts the type / helper layer shared
//! with the legacy `GEORADIUS[BYMEMBER]` family in `radius.rs`.
//!
//! Sub-modules:
//! - `parse` — the arguments, read against the source key as Redis
//!   reads them.

mod parse;

pub(super) use parse::{Form, GeoError, Query, bad_pair, plan};

use kevy_geo::{EARTH_RADIUS_METERS, decode_score, haversine_meters, neighbor_score_ranges};
use kevy_resp::{
    ArgvView, RespVersion, encode_array_len, encode_bulk, encode_double, encode_integer,
};
use kevy_store::{ScoreBound, Store};

/// `GEOSEARCH key <FROMMEMBER member|FROMLONLAT lon lat>
/// <BYRADIUS r unit|BYBOX w h unit> [ASC|DESC] [COUNT n [ANY]]
/// [WITHCOORD] [WITHDIST] [WITHHASH]`
pub(crate) fn cmd_geosearch<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) {
    match plan(store, args, Form::Search) {
        Ok(mut q) => {
            q.opts.proto = proto;
            let hits = run_search(store, &q).unwrap_or_default();
            emit_reply(&hits, &q.opts, out);
        }
        Err(e) => e.emit(&args[0], out),
    }
}

/// Shared search core: fans out over the candidate neighbour ranges
/// around the centre, filters by exact shape, then applies sort + count.
/// A missing source key finds nothing.
pub(super) fn run_search(store: &mut Store, q: &Query) -> Result<Vec<Hit>, kevy_store::StoreError> {
    if q.src_missing {
        return Ok(Vec::new());
    }
    let (clon, clat) = q.opts.center;
    let ranges = neighbor_score_ranges(clon, clat, q.opts.shape.bounding_radius_meters());
    let mut hits = collect_hits(store, &q.src, &ranges, &q.opts)?;
    apply_sort(&mut hits, q.opts.sort);
    apply_count(&mut hits, q.opts.sort, q.opts.count, q.opts.any);
    Ok(hits)
}

/// The search half of a geo `*STORE`: the `(member, score)` pairs the
/// destination ZSet gets. Used by the single-shard dispatch path below and,
/// on a multi-shard server, by the runtime's `Op::GeoSearch` — which runs it
/// on the SOURCE's shard and ships these pairs to the DESTINATION's shard.
pub(super) fn search_pairs(
    store: &mut Store,
    q: &Query,
) -> Result<Vec<(Vec<u8>, f64)>, kevy_store::StoreError> {
    let hits = run_search(store, q)?;
    Ok(store_pairs(&hits, &q.opts))
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

// ───────────── options ─────────────

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
    /// The centre, `(lon, lat)`, its member already looked up.
    center: (f64, f64),
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
    /// The protocol the reply is written in: RESP3 sends coordinates as
    /// doubles.
    pub(super) proto: RespVersion,
}

// ───────────── candidate collection ─────────────

pub(super) struct Hit {
    pub(super) member: Vec<u8>,
    pub(super) score: f64,
    pub(super) dist_m: f64,
}

fn collect_hits(
    store: &mut Store,
    key: &[u8],
    ranges: &[(f64, f64)],
    opts: &Opts,
) -> Result<Vec<Hit>, kevy_store::StoreError> {
    let mut hits = Vec::new();
    for (min, max) in ranges {
        let members =
            store.zrange_by_score(key, ScoreBound::inclusive(*min), ScoreBound::inclusive(*max))?;
        for (member, score) in members {
            if let Some(dist_m) = within(opts.shape, opts.center, decode_score(score)) {
                hits.push(Hit { member, score, dist_m });
            }
        }
    }
    Ok(hits)
}

/// The distance from the centre to `point` when the point is inside the
/// shape, worked out as Redis works it out: a box checks the latitude
/// distance, then the longitude distance at the point's own latitude.
fn within(shape: Shape, (clon, clat): (f64, f64), (plon, plat): (f64, f64)) -> Option<f64> {
    if let Shape::Box { w_m, h_m } = shape {
        let lat_m = EARTH_RADIUS_METERS * (clat.to_radians() - plat.to_radians()).abs();
        if lat_m > h_m / 2.0 || haversine_meters(plon, plat, clon, plat) > w_m / 2.0 {
            return None;
        }
    }
    let d = haversine_meters(clon, clat, plon, plat);
    match shape {
        Shape::Radius { r_m } if d > r_m => None,
        _ => Some(d),
    }
}

fn apply_sort(hits: &mut [Hit], sort: Sort) {
    match sort {
        Sort::Asc => hits.sort_by(|a, b| {
            a.dist_m.partial_cmp(&b.dist_m).expect(
                "GEOADD rejects non-finite coordinates and a non-finite centre matches no cell",
            )
        }),
        Sort::Desc => hits.sort_by(|a, b| {
            b.dist_m.partial_cmp(&a.dist_m).expect(
                "GEOADD rejects non-finite coordinates and a non-finite centre matches no cell",
            )
        }),
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
        hits.sort_by(|a, b| {
            a.dist_m.partial_cmp(&b.dist_m).expect(
                "GEOADD rejects non-finite coordinates and a non-finite centre matches no cell",
            )
        });
    }
    hits.truncate(n);
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
    parsed: &Query,
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
) -> bool {
    let q = match plan(store, args, Form::SearchStore) {
        Ok(q) => q,
        Err(e) => {
            e.emit(&args[0], out);
            return false;
        }
    };
    match search_pairs(store, &q) {
        Ok(pairs) => {
            let dst = &args[1];
            let changed = !pairs.is_empty() || store.key_exists(dst);
            encode_integer(out, store.zstore_result(dst, &pairs) as i64);
            changed
        }
        Err(e) => {
            crate::reply::store_err(out, e);
            false
        }
    }
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
            crate::reply::encode_bulk_fmt(out, format_args!("{:.4}", h.dist_m / opts.unit));
        }
        if opts.with_hash {
            encode_integer(out, h.score as i64);
        }
        if opts.with_coord {
            let (lon, lat) = decode_score(h.score);
            encode_array_len(out, 2);
            emit_coord(out, lon, opts.proto);
            emit_coord(out, lat, opts.proto);
        }
    }
}

/// A coordinate as Redis replies with it: the double's text, as a bulk
/// string under RESP2 and a double under RESP3.
pub(super) fn emit_coord(out: &mut Vec<u8>, v: f64, proto: RespVersion) {
    if proto == RespVersion::V3 {
        return encode_double(out, v);
    }
    kevy_resp::encode_bulk_double(out, v);
}
