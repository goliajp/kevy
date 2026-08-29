//! Radius search — turning a centre and a radius into the score ranges a
//! `ZRANGEBYSCORE` can scan.
//!
//! Split out of `lib.rs` at the 500-line rule. The seam is the question
//! each half answers: `lib.rs` encodes and decodes ONE position, this
//! file answers "which cells could be within `radius_m` of it" — a
//! different job that happens to be built on the same bits.

use crate::{
    GEO_LAT_MAX, GEO_LAT_MIN, GEO_LON_MAX, GEO_LON_MIN, GEO_STEP, interleave52,
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
/// An impossible request degenerates to the whole keyspace rather than to
/// nothing — scanning everything is a slow answer, and returning nothing
/// would be a wrong one:
///
/// ```
/// let all = (0.0, (1u64 << 52) as f64 - 1.0);
/// use kevy_geo::neighbor_score_ranges as r;
/// assert_eq!(r(0.0, 0.0, 0.0), vec![all], "a zero radius");
/// assert_eq!(r(f64::NAN, 0.0, 100.0), vec![all], "an unusable centre");
/// assert_eq!(r(0.0, 0.0, 40_000_000.0), vec![all], "wider than the planet");
/// ```
#[allow(clippy::similar_names)]
pub fn neighbor_score_ranges(lon: f64, lat: f64, radius_m: f64) -> Vec<(f64, f64)> {
    if !lon.is_finite() || !lat.is_finite() || radius_m <= 0.0 {
        return vec![(0.0, (1u64 << 52) as f64 - 1.0)];
    }
    let step = estimate_step(radius_m);
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

fn estimate_step(radius_m: f64) -> u32 {
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
    step.saturating_sub(2).clamp(1, GEO_STEP)
}

fn encode_uniform_step(lon: f64, lat: f64, step: u32) -> (u32, u32) {
    let cells = (1u64 << step) as f64;
    let lat_clamped = lat.clamp(GEO_LAT_MIN, GEO_LAT_MAX);
    let lon_clamped = lon.clamp(GEO_LON_MIN, GEO_LON_MAX);
    let lat_off =
        ((lat_clamped - GEO_LAT_MIN) / (GEO_LAT_MAX - GEO_LAT_MIN) * cells) as u32;
    let lon_off =
        ((lon_clamped - GEO_LON_MIN) / (GEO_LON_MAX - GEO_LON_MIN) * cells) as u32;
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

