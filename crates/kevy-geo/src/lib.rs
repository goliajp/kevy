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
//!
//! ```
//! use kevy_geo::{encode_score, decode_score, haversine_meters, encode_base32_geohash};
//!
//! // The score is what kevy stores in the backing ZSet, so a GEO key is
//! // an ordinary sorted set and every ZSet verb keeps working on it.
//! let palermo = encode_score(13.361389, 38.115556).unwrap();
//! let catania = encode_score(15.087269, 37.502669).unwrap();
//! assert!(palermo != catania);
//!
//! // Decoding is lossy by construction — a 52-bit cell, not a point —
//! // so it round-trips to within the cell, not to the bit.
//! let (lon, lat) = decode_score(palermo);
//! assert!((lon - 13.361389).abs() < 0.001);
//! assert!((lat - 38.115556).abs() < 0.001);
//!
//! // Distance is on the sphere, in metres.
//! let d = haversine_meters(13.361389, 38.115556, 15.087269, 37.502669);
//! assert!((d - 166_274.0).abs() < 100.0);
//!
//! // And the human-facing form Redis reports.
//! let h = encode_base32_geohash(13.361389, 38.115556);
//! assert_eq!(&h[..5], b"sqc8b");
//! ```
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

mod search;
pub use search::neighbor_score_ranges;

/// Encode `(longitude, latitude)` as the 52-bit interleaved geohash
/// stored as a ZSet score. Returns `None` if either coordinate is out
/// of WGS84 range. The score is a non-negative integer ≤ 2⁵² so its
/// f64 representation is exact (within the 53-bit f64 mantissa).
///
/// Bit layout matches Redis: latitude bits at even positions
/// (0, 2, … 50), longitude bits at odd positions (1, 3, … 51).
///
/// # Examples
///
/// Palermo, the fixture Redis's own `GEOADD` documentation uses:
///
/// ```
/// let score = kevy_geo::encode_score(13.361_389_29, 38.115_556_49).unwrap();
/// assert_eq!(score, 3_479_099_956_230_698.0);
/// assert_eq!(score, score as u64 as f64, "exact in f64, so a ZSet can hold it");
/// ```
///
/// Out of range is `None`, not a clamp — a caller that clamped would store
/// a point somewhere the user never named:
///
/// ```
/// use kevy_geo::encode_score;
/// assert!(encode_score(0.0, 86.0).is_none(), "beyond the WGS84 latitude");
/// assert!(encode_score(181.0, 0.0).is_none());
/// assert!(encode_score(f64::NAN, 0.0).is_none());
/// assert!(encode_score(0.0, f64::INFINITY).is_none());
/// ```
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
///
/// # Examples
///
/// The round trip lands on the cell centre, not the original point — a
/// step-26 cell is metres across, and that is the resolution the format
/// has:
///
/// ```
/// use kevy_geo::{encode_score, decode_score};
/// let (lon, lat) = (13.361_389_29, 38.115_556_49);
/// let (dlon, dlat) = decode_score(encode_score(lon, lat).unwrap());
/// assert!((dlon - lon).abs() < 1e-5 && (dlat - lat).abs() < 1e-5);
/// ```
///
/// A score outside the 52-bit space saturates to the WGS84 corner rather
/// than decoding garbage:
///
/// ```
/// let (lon, lat) = kevy_geo::decode_score(-1.0);
/// assert!(lon < -179.99 && lat < -85.05);
/// ```
pub fn decode_score(score: f64) -> (f64, f64) {
    let bits = score_to_bits(score);
    let (ilat, ilon) = deinterleave52(bits);
    cell_centre(ilon, ilat, GEO_LON_MIN, GEO_LON_MAX, GEO_LAT_MIN, GEO_LAT_MAX)
}

/// Great-circle distance in metres between two `(longitude, latitude)`
/// points on the WGS84 sphere (mean radius — matches Redis). Returns
/// `0.0` if the inputs are equal.
/// # Examples
///
/// The Sicily fixture the cross-engine differential uses — Palermo to
/// Catania, the same pair `GEODIST` is checked against.
///
/// ```
/// let d = kevy_geo::haversine_meters(13.361_389, 38.115_556, 15.087_269, 37.502_669);
/// assert!((d - 166_274.0).abs() < 100.0, "got {d}");
/// ```
///
/// A point against itself is zero, not an epsilon.
///
/// ```
/// assert_eq!(kevy_geo::haversine_meters(1.0, 2.0, 1.0, 2.0), 0.0);
/// ```
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
///
/// # Examples
///
/// Byte-equal with what Redis returns for the same point:
///
/// ```
/// let h = kevy_geo::encode_base32_geohash(13.361_389_29, 38.115_556_49);
/// assert_eq!(&h, b"sqc8b49rny0");
/// ```
///
/// The eleventh character is always a zero-padded group — 52 bits do not
/// divide into eleven groups of five — so every hash this returns ends in
/// a character from the low half of the alphabet:
///
/// ```
/// for (lon, lat) in [(0.0, 0.0), (-180.0, -90.0), (179.9, 89.9)] {
///     let h = kevy_geo::encode_base32_geohash(lon, lat);
///     assert_eq!(h[10], b'0', "the pad is not real precision");
/// }
/// ```
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
// similar_names (ilat/ilon, dlat/dlon): the lat/lon pairing is the geo
// domain's standard vocabulary; renaming would hurt, not help.
#[cfg(test)]
mod tests {
    use super::*;

    // Reference values produced by valkey 9.1 + redis 7.4 (both agree) for
    // the canonical Sicily fixture. Regression guards for the two geo
    // encoding bugs the cross-engine differential (`bench/compat3.sh`)
    // caught: GEOHASH 11th-char padding and GEOPOS cell-midpoint rounding.
    const PALERMO: (f64, f64) = (13.361_389, 38.115_556);
    const CATANIA: (f64, f64) = (15.087_269, 37.502_669);

    // `neighbor_score_ranges` had no test at all. The dead-path atlas
    // (bench/DEAD-ATLAS.md) found every one of this crate's four
    // never-executed regions inside it, which is what a public function
    // with zero direct coverage looks like from the outside: exercised
    // through the GEO commands, never at its own edges.

    /// A radius past the Mercator span collapses the step to 1, and the
    /// function answers with the whole keyspace rather than nine cells.
    /// Reaching that branch needs `radius_m >= MERCATOR_MAX`, which no
    /// caller had ever passed.
    #[test]
    fn a_radius_larger_than_the_world_returns_the_whole_range() {
        let whole = (0.0, (1u64 << 52) as f64 - 1.0);
        let r = neighbor_score_ranges(PALERMO.0, PALERMO.1, 3.0e7);
        assert_eq!(r, vec![whole], "a radius past MERCATOR_MAX must not be tiled");

        // Just under it still tiles, so the boundary is the reason for the
        // answer and not an artefact of the size.
        let tiled = neighbor_score_ranges(PALERMO.0, PALERMO.1, 1.0e6);
        assert_ne!(tiled, vec![whole], "a radius inside the world must tile");
        assert!(!tiled.is_empty());
    }

    /// Near the latitude limit the 3x3 neighbourhood runs off the top of
    /// the grid, and those cells are skipped rather than wrapped —
    /// longitude wraps, latitude does not. Nothing had exercised the skip.
    #[test]
    fn cells_past_the_latitude_limit_are_skipped_not_wrapped() {
        let polar = neighbor_score_ranges(0.0, GEO_LAT_MAX - 0.000_01, 100.0);
        let middle = neighbor_score_ranges(0.0, 0.0, 100.0);
        assert!(!polar.is_empty(), "the pole still yields its own cells");
        assert!(
            polar.len() <= middle.len(),
            "a neighbourhood clipped by the pole cannot exceed a full one: \
             polar {} vs middle {}",
            polar.len(),
            middle.len()
        );
        for (lo, hi) in &polar {
            assert!(lo <= hi, "each range is ordered");
            assert!(*lo >= 0.0, "no range escapes the keyspace");
        }
    }

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
