//! Geo `*STORE` orchestrator (`GEOSEARCHSTORE`, `GEORADIUS … STORE|STOREDIST`,
//! `GEORADIUSBYMEMBER … STORE|STOREDIST`), step-1→step-2 transition.
//!
//! Same two-hop shape as [`crate::exec_zalgebra`], with one difference: the
//! search itself is a command-layer concern (geohash decoding, radius/box
//! geometry, unit handling), so step 1 ships the whole argv to the SOURCE
//! key's shard and runs it through [`crate::Commands::geo_search`] there,
//! rather than gathering the raw zset to the origin. Step 2 is the shared
//! `Op::ZStoreResult` — the destination's own shard DELs and rewrites it,
//! folding the cardinality through a re-armed `Agg::SumInt` into `:n`.
//!
//! Routing this rather than leaving it to the catch-all `Route::Single(1)`
//! is the whole point: `GEOSEARCHSTORE` puts the DESTINATION at argv[1] and
//! `GEORADIUS` puts the SOURCE there, so a single hashed-argv[1] route sends
//! one of the two keys to a shard that doesn't own it.

use crate::Commands;
use crate::message::{Agg, Op};
use crate::shard::Shard;

/// What a geo `*STORE`'s search phase produced on the source key's shard.
/// Public: [`crate::Commands::geo_search`] returns it. The runtime never
/// interprets the scores — the command layer decides whether they carry the
/// source geohash or (with `STOREDIST`) the distance in the queried unit.
pub enum GeoHits {
    /// `(member, score)` pairs to materialize at the destination. Empty =
    /// the search matched nothing, which deletes the destination (Redis
    /// leaves no key behind on an empty result).
    Pairs(Vec<(Vec<u8>, f64)>),
    /// Pre-encoded RESP error frame: a syntax error, a WRONGTYPE source, or
    /// a `FROMMEMBER` / `GEORADIUSBYMEMBER` anchor the source doesn't hold.
    Error(Vec<u8>),
}

impl<C: Commands> Shard<C> {
    /// Step 1 landed: the source shard answered with the hits (or an error).
    /// Ship the write to the destination's owning shard.
    pub(crate) fn finalize_geostore_agg(&mut self, conn_id: u64, seq: u64, agg: Agg) {
        let Agg::GeoStore { dst, hits } = agg else {
            return;
        };
        let pairs = match hits {
            Some(GeoHits::Pairs(p)) => p,
            Some(GeoHits::Error(reply)) => {
                self.fill_zstore_slot(conn_id, seq, reply);
                return;
            }
            // The source shard folded a Part this agg can't pair with —
            // a runtime bug. Reply an error rather than wedge the slot.
            None => {
                self.fill_zstore_slot(
                    conn_id,
                    seq,
                    b"-ERR internal: geo store search produced no result\r\n".to_vec(),
                );
                return;
            }
        };
        let dst_shard = self.shard_of(&dst);
        self.ship_store_op(conn_id, seq, dst_shard, Op::ZStoreResult { dst, pairs });
    }
}
