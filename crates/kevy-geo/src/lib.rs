//! `kevy-geo` — pure-Rust, zero-dependency primitives for Redis-style
//! GEO commands. Provides geohash encoding (score-form, used as the ZSet
//! score that backs every GEO key in kevy) and the standard 11-character
//! base32 string geohash, plus the WGS84 great-circle distance kevy needs
//! for `GEODIST` / `GEOSEARCH BYRADIUS`. Implementation deliberately
//! matches Redis bit-for-bit so kevy GEO keys are wire-interchangeable
//! with valkey clients.
//!
//! What it is NOT:
//! - A full geo library — no projection conversions, no datums other than
//!   WGS84, no path/intersection geometry, no R-tree spatial index. The
//!   Redis-style GEO API is intentionally narrow; this crate matches that
//!   narrowness rather than trying to be `proj` or `geo-types`.
//! - A `no_std` crate — uses `f64::sqrt`/`sin`/`cos`/`atan2` from `std`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Inclusive latitude bound (degrees). Matches Redis's Web Mercator
/// limit — the encoding cannot represent the poles because Web Mercator
/// maps them to ±∞.
pub const GEO_LAT_MIN: f64 = -85.051_128_78;
/// Inclusive latitude bound (degrees).
pub const GEO_LAT_MAX: f64 = 85.051_128_78;
/// Inclusive longitude bound (degrees).
pub const GEO_LON_MIN: f64 = -180.0;
/// Inclusive longitude bound (degrees).
pub const GEO_LON_MAX: f64 = 180.0;
/// Mean great-circle Earth radius in metres, matching Redis's constant
/// exactly (`6_372_797.560_856`). Used by [`haversine_meters`].
pub const EARTH_RADIUS_METERS: f64 = 6_372_797.560_856;
/// Bits per axis in the 52-bit interleaved score. Matches Redis.
pub const GEO_STEP: u32 = 26;

/// Encode `(longitude, latitude)` as the 52-bit interleaved geohash
/// stored as a ZSet score. Returns `None` if either coordinate is out
/// of WGS84 range. The score is a non-negative integer ≤ 2⁵² so its
/// f64 representation is exact (within the 53-bit f64 mantissa).
///
/// Bit layout matches Redis: latitude bits at even positions
/// (0, 2, … 50), longitude bits at odd positions (1, 3, … 51).
pub fn encode_score(lon: f64, lat: f64) -> Option<f64> {
    if !(lon.is_finite() && lat.is_finite()) {
        return None;
    }
    if !(GEO_LAT_MIN..=GEO_LAT_MAX).contains(&lat) {
        return None;
    }
    if !(GEO_LON_MIN..=GEO_LON_MAX).contains(&lon) {
        return None;
    }
    let bits = encode_bits_wgs84(lon, lat);
    Some(bits as f64)
}

/// Inverse of [`encode_score`]: decode a ZSet score back to the
/// `(longitude, latitude)` *centre* of its geohash cell. Out-of-range
/// scores saturate to the WGS84 bounds rather than producing garbage.
pub fn decode_score(score: f64) -> (f64, f64) {
    let bits = score_to_bits(score);
    let (ilat, ilon) = deinterleave52(bits);
    cell_centre(ilon, ilat, GEO_LON_MIN, GEO_LON_MAX, GEO_LAT_MIN, GEO_LAT_MAX)
}

/// Great-circle distance in metres between two `(longitude, latitude)`
/// points on the WGS84 sphere (mean radius — matches Redis). Returns
/// `0.0` if the inputs are equal.
pub fn haversine_meters(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lon2 - lon1).to_radians();
    let a = (dphi * 0.5).sin().powi(2)
        + phi1.cos() * phi2.cos() * (dlam * 0.5).sin().powi(2);
    let c = 2.0 * a.sqrt().clamp(0.0, 1.0).asin();
    EARTH_RADIUS_METERS * c
}

/// Encode `(lon, lat)` as the 11-character base32 geohash string Redis
/// returns from `GEOHASH`. Uses the **standard** lat range [-90, 90]
/// (NOT the WGS84 ±85.05 range used for the score). The high 55 bits
/// of a step-26 standard-range encoding are emitted in 5-bit groups;
/// the low 3 bits of the 11th char are always zero (52 ÷ 5 = 10 r 2).
pub fn encode_base32_geohash(lon: f64, lat: f64) -> [u8; 11] {
    const ALPHABET: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let bits = encode_bits_full_range(lon, lat);
    let mut out = [0u8; 11];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 52i32 - (i as i32 + 1) * 5;
        // Redis emits the 52 score bits across the first 10 chars (50 bits)
        // and pads the 11th char with zero rather than spilling the 2 low
        // real bits into it — match that so GEOHASH strings are byte-equal.
        let idx = if shift >= 0 {
            ((bits >> shift) & 0x1f) as usize
        } else {
            0
        };
        *slot = ALPHABET[idx];
    }
    out
}

/// Bit-wise interleave: bits of `lat_u32` at even positions, bits of
/// `lon_u32` at odd positions, producing the 52-bit score layout. Only
/// the low 26 bits of each input contribute.
fn interleave52(lat: u32, lon: u32) -> u64 {
    spread26(u64::from(lat)) | (spread26(u64::from(lon)) << 1)
}

/// Inverse of [`interleave52`]: extract `(lat_u32, lon_u32)` (26 bits
/// each) from a 52-bit interleaved value.
fn deinterleave52(bits: u64) -> (u32, u32) {
    let lat = pack26(bits) as u32;
    let lon = pack26(bits >> 1) as u32;
    (lat, lon)
}

/// Spread the low 26 bits of `x` into the even bit positions of a
/// 52-bit result (Bit Twiddling Hacks: interleave-by-magic-numbers).
fn spread26(mut x: u64) -> u64 {
    x &= 0x3ff_ffff; // 26 bits
    x = (x | (x << 16)) & 0x0000_0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x0000_00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x000F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    x = (x | (x << 1)) & 0x5555_5555_5555_5555;
    x
}

/// Inverse of [`spread26`]: collapse the even-positioned bits of a
/// 52-bit value back into a contiguous 26-bit integer.
fn pack26(mut x: u64) -> u64 {
    x &= 0x5555_5555_5555_5555;
    x = (x | (x >> 1)) & 0x3333_3333_3333_3333;
    x = (x | (x >> 2)) & 0x000F_0F0F_0F0F_0F0F;
    x = (x | (x >> 4)) & 0x0000_00FF_00FF_00FF_00FF;
    x = (x | (x >> 8)) & 0x0000_0000_FFFF_0000_FFFF;
    x = (x | (x >> 16)) & 0x3ff_ffff;
    x
}

fn encode_bits_wgs84(lon: f64, lat: f64) -> u64 {
    encode_bits(lon, lat, GEO_LON_MIN, GEO_LON_MAX, GEO_LAT_MIN, GEO_LAT_MAX)
}

fn encode_bits_full_range(lon: f64, lat: f64) -> u64 {
    encode_bits(lon, lat, GEO_LON_MIN, GEO_LON_MAX, -90.0, 90.0)
}

fn encode_bits(
    lon: f64,
    lat: f64,
    lon_min: f64,
    lon_max: f64,
    lat_min: f64,
    lat_max: f64,
) -> u64 {
    let cells = (1u64 << GEO_STEP) as f64;
    let lat_off = ((lat - lat_min) / (lat_max - lat_min)) * cells;
    let lon_off = ((lon - lon_min) / (lon_max - lon_min)) * cells;
    let lat_u = (lat_off as u32).min((1 << GEO_STEP) - 1);
    let lon_u = (lon_off as u32).min((1 << GEO_STEP) - 1);
    interleave52(lat_u, lon_u)
}

fn cell_centre(
    ilon: u32,
    ilat: u32,
    lon_min: f64,
    lon_max: f64,
    lat_min: f64,
    lat_max: f64,
) -> (f64, f64) {
    // Mirror Redis's geohashDecode float-op order EXACTLY: decode each axis
    // to its cell [min,max] separately, then average — `lon_min + (i/cells)
    // *span` for min and `(i+1)/cells*span` for max. The mathematically
    // equivalent `(i+0.5)/cells*span` rounds differently in the last ULP,
    // which made GEOPOS diverge from valkey/redis in the final digits.
    let cells = (1u64 << GEO_STEP) as f64;
    let lon_span = lon_max - lon_min;
    let lat_span = lat_max - lat_min;
    let lon_lo = lon_min + (f64::from(ilon) / cells) * lon_span;
    let lon_hi = lon_min + ((f64::from(ilon) + 1.0) / cells) * lon_span;
    let lat_lo = lat_min + (f64::from(ilat) / cells) * lat_span;
    let lat_hi = lat_min + ((f64::from(ilat) + 1.0) / cells) * lat_span;
    (f64::midpoint(lon_lo, lon_hi), f64::midpoint(lat_lo, lat_hi))
}

/// Convert an f64 score back to its 52-bit interleaved integer. Saturates
/// negative / NaN / >2⁵² values to the valid range so that `decode_score`
/// on a garbage score still produces a defined `(lon, lat)` pair rather
/// than UB or a wild f64 cast.
fn score_to_bits(score: f64) -> u64 {
    if !score.is_finite() || score < 0.0 {
        return 0;
    }
    let max = (1u64 << (GEO_STEP * 2)) - 1;
    let n = score as u64;
    n.min(max)
}

// ───────────── neighbor score ranges ─────────────

/// Compute up to 9 ZSet-score ranges (closed-inclusive `(min, max)` as
/// f64-encoded 52-bit integers) that cover **at least** the disc of
/// `radius_m` metres around `(lon, lat)`. Each range maps a step-`k`
/// geohash cell to its contiguous score interval in the step-26 layout.
///
/// Returns the ranges sorted by `min`, with adjacent ranges merged so
/// the caller can iterate them as `ZRANGEBYSCORE` queries without
/// redundant work. The set over-approximates the circle by at most one
/// cell width — callers MUST filter by exact distance afterwards.
///
/// Returns a single all-key range `(0, 2⁵² − 1)` when the radius is
/// large enough to span the globe or the centre is invalid.
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

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values produced by valkey 9.1 + redis 7.4 (both agree) for
    // the canonical Sicily fixture. Regression guards for the two geo
    // encoding bugs the cross-engine differential (`bench/compat3.sh`)
    // caught: GEOHASH 11th-char padding and GEOPOS cell-midpoint rounding.
    const PALERMO: (f64, f64) = (13.361_389, 38.115_556);
    const CATANIA: (f64, f64) = (15.087_269, 37.502_669);

    #[test]
    fn geohash_string_matches_redis() {
        // 11th char must be '0' (zero-padded), not the spilled low bits.
        assert_eq!(&encode_base32_geohash(PALERMO.0, PALERMO.1), b"sqc8b49rny0");
        assert_eq!(&encode_base32_geohash(CATANIA.0, CATANIA.1), b"sqdtr74hyu0");
    }

    #[test]
    fn geopos_roundtrip_matches_redis_to_the_last_digit() {
        // decode(encode(..)) must reproduce valkey/redis GEOPOS byte-for-byte
        // (17 sig digits), which requires the exact cell-midpoint float order.
        for (lon, lat, want_lon, want_lat) in [
            (
                PALERMO.0,
                PALERMO.1,
                "13.36138933897018433",
                "38.11555639549629859",
            ),
            (
                CATANIA.0,
                CATANIA.1,
                "15.08726745843887329",
                "37.50266842333162032",
            ),
        ] {
            let score = encode_score(lon, lat).expect("in range");
            let (dlon, dlat) = decode_score(score);
            assert_eq!(format!("{dlon:.17}"), want_lon, "lon for ({lon},{lat})");
            assert_eq!(format!("{dlat:.17}"), want_lat, "lat for ({lon},{lat})");
        }
    }
}
