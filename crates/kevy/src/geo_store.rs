//! Cross-shard glue for the geo `*STORE` family.
//!
//! `GEOSEARCHSTORE dst src …` and `GEORADIUS[BYMEMBER] src … STORE|STOREDIST
//! dst` read one key and write another, and the two keys hash to different
//! shards. [`geo_store_route`] hands the runtime both keys, and [`geo_search`]
//! is the read half it calls back on the SOURCE's shard — the write then lands
//! on the DESTINATION's shard as a plain ZSet materialisation. The argv
//! reading and the search itself are `kevy_verbs::geo`'s.

use kevy_resp::ArgvView;
use kevy_rt::{GeoHits, Route};
use kevy_store::Store;

/// `Some(Route::GeoStore { .. })` for a geo command that writes a destination
/// key; `None` for every other shape (including the query-only geo forms) —
/// the caller keeps its normal single-key route for those.
pub(crate) fn geo_store_route<A: ArgvView + ?Sized>(verb: &[u8], args: &A) -> Option<Route> {
    kevy_verbs::geo::store_keys(verb, args).map(|(src, dst)| Route::GeoStore { src, dst })
}

/// Run a geo `*STORE`'s search against the SOURCE key (this shard owns it) and
/// return the `(member, score)` pairs the destination's shard will write.
pub(crate) fn geo_search(store: &mut Store, argv: &[Vec<u8>]) -> GeoHits {
    match kevy_verbs::geo::store_search(store, argv) {
        Ok(pairs) => GeoHits::Pairs(pairs),
        Err(reply) => GeoHits::Error(reply),
    }
}
