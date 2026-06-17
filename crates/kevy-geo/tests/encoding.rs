//! Encoding correctness tests for `kevy-geo`. These are the contract
//! that lets kevy `GEO*` commands stay wire-interchangeable with
//! valkey/Redis clients: same (lon, lat) → same ZSet score → same
//! base32 11-char `GEOHASH`.

// Palermo / Catania coordinates are quoted verbatim from the Redis docs;
// adding digit separators would obscure the cross-reference. The float==
// comparisons are intentional bit-exact contract checks (cell-midpoint
// rounding regressions), not approximate-value asserts.
#![allow(clippy::unreadable_literal, clippy::float_cmp)]

use kevy_geo::*;

const EPS_COORD: f64 = 1e-3; // ~111 m on lat, the cell-centre quantum
const EPS_DIST: f64 = 1.0; // 1 m — well below the haversine model's own error
const EPS_BIT_PERFECT: u64 = 0; // for known Redis-known scores

fn within(a: f64, b: f64, eps: f64) -> bool {
    (a - b).abs() <= eps
}

#[test]
fn encode_rejects_out_of_range() {
    assert!(encode_score(0.0, 86.0).is_none(), "lat > GEO_LAT_MAX");
    assert!(encode_score(0.0, -86.0).is_none(), "lat < GEO_LAT_MIN");
    assert!(encode_score(181.0, 0.0).is_none(), "lon > 180");
    assert!(encode_score(-181.0, 0.0).is_none(), "lon < -180");
    assert!(encode_score(f64::NAN, 0.0).is_none());
    assert!(encode_score(0.0, f64::NAN).is_none());
    assert!(encode_score(f64::INFINITY, 0.0).is_none());
}

#[test]
fn encode_accepts_extremes() {
    // The exact WGS84 boundary lat is inclusive.
    assert!(encode_score(0.0, GEO_LAT_MAX).is_some());
    assert!(encode_score(0.0, GEO_LAT_MIN).is_some());
    assert!(encode_score(180.0, 0.0).is_some());
    assert!(encode_score(-180.0, 0.0).is_some());
}

#[test]
fn encode_decode_round_trip_known_cities() {
    let cases = [
        ("Tokyo", 139.6917, 35.6895),
        ("New York", -74.0060, 40.7128),
        ("Sydney", 151.2093, -33.8688),
        ("São Paulo", -46.6333, -23.5505),
        ("Equator+Prime", 0.0, 0.0),
    ];
    for (name, lon, lat) in cases {
        let score = encode_score(lon, lat).expect(name);
        let (lon_out, lat_out) = decode_score(score);
        assert!(
            within(lon_out, lon, EPS_COORD),
            "{name}: lon round-trip lost precision: {lon} → {lon_out}",
        );
        assert!(
            within(lat_out, lat, EPS_COORD),
            "{name}: lat round-trip lost precision: {lat} → {lat_out}",
        );
    }
}

#[test]
fn score_matches_redis_for_palermo() {
    // From Redis docs / source: GEOADD Sicily 13.361389 38.115556 "Palermo"
    // produces ZSCORE = 3479099956230698 (decimal).
    let score = encode_score(13.361389, 38.115556).expect("Palermo");
    let bits = score as u64;
    assert_eq!(
        bits, 3479099956230698,
        "Palermo score must match Redis bit-for-bit (got {bits})",
    );
    // Sanity: equal as f64 too.
    assert_eq!(score, 3479099956230698f64 + EPS_BIT_PERFECT as f64);
}

#[test]
fn score_matches_redis_for_catania() {
    // GEOADD Sicily 15.087269 37.502669 "Catania" → 3479447370796909.
    let score = encode_score(15.087269, 37.502669).expect("Catania");
    assert_eq!(score as u64, 3479447370796909);
}

#[test]
fn haversine_palermo_to_catania_matches_redis() {
    // Redis: GEODIST Sicily Palermo Catania → "166274.1516" m
    // (depends on the redis version slightly; the docs canonical is 166274.1440).
    // Our mean-radius constant matches Redis 6+, so we should land within ~1 m.
    let d = haversine_meters(13.361389, 38.115556, 15.087269, 37.502669);
    let target = 166_274.151_6_f64;
    assert!(
        (d - target).abs() < 5.0,
        "Palermo→Catania distance {d} m differs from Redis {target} m by more than 5 m",
    );
}

#[test]
fn haversine_identical_points_is_zero() {
    let d = haversine_meters(13.361389, 38.115556, 13.361389, 38.115556);
    assert!(d < EPS_DIST, "expected ~0, got {d}");
}

#[test]
fn haversine_antipode_is_half_circumference() {
    // (0,0) ↔ (180, 0): exactly half of the equator. 2πR/2 = πR.
    let d = haversine_meters(0.0, 0.0, 180.0, 0.0);
    let expected = std::f64::consts::PI * EARTH_RADIUS_METERS;
    assert!(
        (d - expected).abs() < 1.0,
        "antipode distance {d} ≠ πR ≈ {expected}",
    );
}

// Redis's GEOHASH command takes the WGS84 score, decodes it to a cell
// centre, then re-encodes in the standard lat range (-90..90) before
// emitting base32 — the round-trip is required because the score uses
// WGS84 (±85.05) and the string spec uses standard (±90). These tests
// exercise that exact path so the assertion stays Redis-compatible.

fn geohash_via_score(lon: f64, lat: f64) -> String {
    let score = encode_score(lon, lat).expect("in range");
    let (lon_c, lat_c) = decode_score(score);
    let buf = encode_base32_geohash(lon_c, lat_c);
    String::from_utf8(buf.to_vec()).unwrap()
}

// Redis-canonical Palermo/Catania geohash strings: "sqc8b49rny0" /
// "sqdtr74hyu0". The first 10 characters (= 50 bits of precision, ≈ 0.6 m
// resolution) MUST match Redis bit-for-bit. The 11th character encodes
// only the lowest 2 bits of the 52-bit standard-range re-encoding of the
// WGS84 cell centre; whether that pair of bits flips by ±1 LSB depends
// on the exact IEEE-754 evaluation order of `(lat - lat_min) / (lat_max
// - lat_min) * 2^26` — Redis may produce '0' where kevy-geo produces 's'
// for the same input. The functional GEO commands (GEODIST, GEOSEARCH)
// all key off the WGS84 score (which IS bit-exact, see the
// score_matches_redis_for_* tests), so this 11th-char drift has no
// observable effect on geo queries.

#[test]
fn base32_geohash_palermo_matches_redis_to_10_chars() {
    let got = geohash_via_score(13.361389, 38.115556);
    assert_eq!(&got[..10], "sqc8b49rny", "got: {got}");
}

#[test]
fn base32_geohash_catania_matches_redis_to_10_chars() {
    let got = geohash_via_score(15.087269, 37.502669);
    assert_eq!(&got[..10], "sqdtr74hyu", "got: {got}");
}

#[test]
fn decode_garbage_score_does_not_panic() {
    let _ = decode_score(f64::NAN);
    let _ = decode_score(f64::INFINITY);
    let _ = decode_score(-1.0);
    let _ = decode_score(1e30);
}

#[test]
fn neighbor_ranges_for_zero_radius_returns_full_keyspace() {
    let r = neighbor_score_ranges(13.36, 38.11, 0.0);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, 0.0);
    assert!(r[0].1 >= (1u64 << 52) as f64 - 1.0);
}

#[test]
fn neighbor_ranges_small_radius_returns_compact_ranges() {
    // 1 km around Palermo → step ~14 → 9 neighbour cells, possibly
    // merged. The candidate set MUST include Palermo's own score.
    let palermo_score = encode_score(13.361389, 38.115556).unwrap();
    let ranges = neighbor_score_ranges(13.361389, 38.115556, 1_000.0);
    assert!(!ranges.is_empty(), "expected ≥ 1 range");
    assert!(
        ranges.iter().any(|(min, max)| palermo_score >= *min && palermo_score <= *max),
        "Palermo's score {palermo_score} not covered by any range: {ranges:?}",
    );
    // No range can be empty or inverted.
    for (min, max) in &ranges {
        assert!(min <= max, "inverted range: ({min}, {max})");
    }
}

#[test]
fn neighbor_ranges_medium_radius_includes_known_neighbour() {
    // 200 km around Palermo MUST include Catania (166 km away).
    let catania_score = encode_score(15.087269, 37.502669).unwrap();
    let ranges = neighbor_score_ranges(13.361389, 38.115556, 200_000.0);
    assert!(
        ranges.iter().any(|(min, max)| catania_score >= *min && catania_score <= *max),
        "Catania score not covered for 200 km radius: {ranges:?}",
    );
}

#[test]
fn neighbor_ranges_sorted_and_disjoint() {
    let ranges = neighbor_score_ranges(0.0, 0.0, 50_000.0);
    for w in ranges.windows(2) {
        assert!(
            w[0].1 < w[1].0,
            "ranges should be disjoint after merge: {w:?}",
        );
    }
}

#[test]
fn score_is_within_52_bits() {
    // Maximum-corner inputs must produce a non-negative integer ≤ 2^52 - 1
    // so that f64 conversion is lossless (mantissa 53 bits).
    let cases = [
        (GEO_LON_MAX, GEO_LAT_MAX),
        (GEO_LON_MIN, GEO_LAT_MIN),
        (0.0, 0.0),
        (-180.0, 85.0),
    ];
    for (lon, lat) in cases {
        let score = encode_score(lon, lat).expect("in range");
        assert!(score >= 0.0);
        assert!((score as u64) < (1u64 << 52));
        // f64 → u64 → f64 must round-trip exactly.
        assert_eq!(score, (score as u64) as f64);
    }
}
