//! Radius search — turning a centre and a radius into the score ranges a
//! `ZRANGEBYSCORE` can scan.
//!
//! Split out of `lib.rs` at the 500-line rule. The seam is the question
//! each half answers: `lib.rs` encodes and decodes ONE position, this
//! file answers "which cells could be within `radius_m` of it" — a
//! different job that happens to be built on the same bits.

use crate::{
    EARTH_RADIUS_METERS, GEO_LAT_MAX, GEO_LAT_MIN, GEO_LON_MAX, GEO_LON_MIN, GEO_STEP, interleave52,
};

///
/// # Examples
///
/// The ranges over-approximate the circle, so a caller must still filter
/// by real distance — this narrows the ZSet scan, it does not answer the
/// query:
///
/// ```
/// let ranges = kevy_geo::neighbor_score_ranges(13.361_389_29, 38.115_556_49, 200_000.0);
/// assert!(!ranges.is_empty());
/// for (lo, hi) in &ranges {
///     assert!(lo <= hi, "each range is ordered");
/// }
/// ```
///
/// An unusable centre degenerates to the whole keyspace rather than to
/// nothing — scanning everything is a slow answer, and returning nothing
/// would be a wrong one. A radius wider than the planet does the same,
/// because it genuinely covers everything.
///
/// A zero radius does NOT. It used to, and that made
/// `GEOSEARCH … BYRADIUS 0` a scan of every member from one client
/// command — measured at 9.23 ms against 0.05 ms for `BYRADIUS 1000` on
/// the same 200,000-member key.
///
/// ```
/// let all = (0.0, (1u64 << 52) as f64 - 1.0);
/// use kevy_geo::neighbor_score_ranges as r;
/// assert_eq!(r(f64::NAN, 0.0, 100.0), vec![all], "an unusable centre");
/// assert_eq!(r(0.0, 0.0, 40_000_000.0), vec![all], "wider than the planet");
///
/// // A zero radius is the smallest cell there is, not the largest.
/// let zero: f64 = r(0.0, 0.0, 0.0).iter().map(|(lo, hi)| hi - lo).sum();
/// assert!(zero < all.1 / 1e6, "a zero radius must not scan the keyspace");
/// ```
#[allow(clippy::similar_names)]
pub fn neighbor_score_ranges(lon: f64, lat: f64, radius_m: f64) -> Vec<(f64, f64)> {
    if !lon.is_finite() || !lat.is_finite() {
        return vec![(0.0, (1u64 << 52) as f64 - 1.0)];
    }
    // A zero or negative radius used to short-circuit to the whole
    // keyspace here — which made `GEOSEARCH … BYRADIUS 0` a full scan of
    // the key, O(members), from one client command. Measured on a
    // 200,000-member key: 9.23 ms against 0.05 ms for `BYRADIUS 1000`,
    // on the same key and the same connection.
    //
    // `estimate_step` already answers this case correctly — it returns
    // `GEO_STEP` for a non-positive radius, which is a single 52-bit
    // cell. The guard above was overriding a right answer that already
    // existed two functions down.
    let step = estimate_step(radius_m, lat);
    if step <= 1 {
        return vec![(0.0, (1u64 << 52) as f64 - 1.0)];
    }
    let (clat, clon) = encode_uniform_step(lon, lat, step);
    let mut raw: Vec<(u64, u64)> = Vec::with_capacity(9);
    let cells = 1i32 << step;
    let shift = (GEO_STEP - step) * 2;
    let inner_mask = (1u64 << shift) - 1;
    for dlat in -1i32..=1 {
        for dlon in -1i32..=1 {
            let ilat = clat as i32 + dlat;
            if !(0..cells).contains(&ilat) {
                continue;
            }
            let ilon = (clon as i32 + dlon).rem_euclid(cells);
            let prefix = interleave52(ilat as u32, ilon as u32);
            let min = prefix << shift;
            let max = min | inner_mask;
            raw.push((min, max));
        }
    }
    raw.sort_unstable();
    merge_ranges(raw)
}

/// The cell size to search at, for a radius **at a latitude**.
///
/// The latitude is not decoration. A cell's longitude width in degrees is
/// fixed, but the bounding box's longitude half-width is
/// `rad_deg(r / EARTH_R) / cos(lat)` — which diverges as the pole is
/// approached. So the nine cells that cover the box at the equator stop
/// covering it further north, and members inside the radius are simply
/// never looked at.
///
/// Measured before this took a latitude, placing 360 points at 98 % of
/// the radius and asking for all of them back:
///
/// | latitude | radius | returned of 360 |
/// |---|---|---|
/// | 0, 60, 66 | 1 km | 360 |
/// | 70 | 1 km | 297 |
/// | 80 | 1 km | 171 |
/// | 84 | 1 km | **94** |
/// | −80 | 100 km | 247 |
///
/// Redis widens the cells at two latitudes for exactly this, and those
/// two thresholds are what put the guarantee back past the latitudes
/// where the loss above starts.
fn estimate_step(radius_m: f64, lat: f64) -> u32 {
    const MERCATOR_MAX: f64 = 20_037_726.37;
    if radius_m <= 0.0 {
        return GEO_STEP;
    }
    let mut step = 1u32;
    let mut r = radius_m;
    while r < MERCATOR_MAX {
        r *= 2.0;
        step += 1;
    }
    let mut step = step.saturating_sub(2).clamp(1, GEO_STEP);
    // Widen until nine cells provably cover the box.
    //
    // Redis does this with two latitude thresholds plus a correction
    // pass. The condition underneath both is checkable directly, so this
    // checks it: the query point can sit anywhere in its cell, including
    // exactly on an edge, so the only margin the 3x3 block guarantees on
    // any side is one whole cell. Nine cells therefore cover the box
    // exactly when each half-extent fits within one cell's span.
    //
    // The longitude half-extent is where latitude enters — it is the
    // latitude half-extent divided by `cos(lat)`, which is what grows
    // without bound toward the pole. `lat` is clamped to the Mercator
    // limit first, so the divisor cannot reach zero.
    let (lat_half, lon_half) = box_half_extents(radius_m, lat);
    while step > 1 && !nine_cells_cover(step, lat_half, lon_half) {
        step -= 1;
    }
    step
}

/// Half-height and half-width of the bounding box, in degrees.
///
/// The width is taken at the box's POLE-MOST latitude, not its centre.
/// A circle on a sphere is widest in longitude at whichever of its edges
/// is nearer the pole, and using the centre's latitude under-measures
/// that by `cos(centre) / cos(edge)` — small near the equator, unbounded
/// near the pole. Measured at latitude 84 with a 500 km radius, the
/// centre-latitude version still lost 2 of 63 members that were inside
/// the radius.
///
/// The pole-most latitude is clamped to the Mercator limit because no
/// score exists beyond it, which also keeps the divisor away from zero.
fn box_half_extents(radius_m: f64, lat: f64) -> (f64, f64) {
    let lat_half = (radius_m / EARTH_RADIUS_METERS).to_degrees();
    let widest = (lat.abs() + lat_half).min(GEO_LAT_MAX);
    let lon_half = lat_half / widest.to_radians().cos();
    (lat_half, lon_half)
}

/// Whether one cell at `step` is at least as large as the box's
/// half-extents — the condition under which the centre cell plus its
/// eight neighbours cover the box for any position of the point inside
/// the centre cell.
fn nine_cells_cover(step: u32, lat_half: f64, lon_half: f64) -> bool {
    let cells = (1u64 << step) as f64;
    let lat_span = (GEO_LAT_MAX - GEO_LAT_MIN) / cells;
    let lon_span = 360.0 / cells;
    lat_span >= lat_half && lon_span >= lon_half
}

fn encode_uniform_step(lon: f64, lat: f64, step: u32) -> (u32, u32) {
    let cells = (1u64 << step) as f64;
    let lat_clamped = lat.clamp(GEO_LAT_MIN, GEO_LAT_MAX);
    let lon_clamped = lon.clamp(GEO_LON_MIN, GEO_LON_MAX);
    let lat_off = ((lat_clamped - GEO_LAT_MIN) / (GEO_LAT_MAX - GEO_LAT_MIN) * cells) as u32;
    let lon_off = ((lon_clamped - GEO_LON_MIN) / (GEO_LON_MAX - GEO_LON_MIN) * cells) as u32;
    let max = (1u32 << step) - 1;
    (lat_off.min(max), lon_off.min(max))
}

/// Sort + coalesce adjacent / overlapping integer ranges, then convert
/// to the `(f64, f64)` form callers feed into `ZRANGEBYSCORE`. The 52-bit
/// integer ↔ f64 mapping is exact within the f64 mantissa.
fn merge_ranges(sorted: Vec<(u64, u64)>) -> Vec<(f64, f64)> {
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(sorted.len());
    for (min, max) in sorted {
        match out.last_mut() {
            Some(prev) if prev.1.saturating_add(1) >= min => {
                prev.1 = prev.1.max(max);
            }
            _ => out.push((min, max)),
        }
    }
    out.into_iter().map(|(a, b)| (a as f64, b as f64)).collect()
}
