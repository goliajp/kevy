# kevy-geo

Pure-Rust, zero-dependency geohash + great-circle distance primitives for
Redis-style GEO commands. Used by the [kevy](https://crates.io/crates/kevy)
server to back `GEOADD` / `GEOPOS` / `GEODIST` / `GEOHASH` / `GEOSEARCH`,
but the crate is self-contained and reusable.

- 52-bit interleaved geohash matching Redis's score layout
  (`encode_score` / `decode_score`)
- 11-character base32 string geohash (`encode_base32_geohash`)
- Great-circle distance on the WGS84 mean-radius sphere
  (`haversine_meters`)

## Charter

`#![forbid(unsafe_code)]`, zero crates.io dependencies, `std`-only (uses
`f64` math). Intentionally narrow — no projections, no datums other than
WGS84, no spatial index.

## License

Dual-licensed under Apache-2.0 OR MIT.
