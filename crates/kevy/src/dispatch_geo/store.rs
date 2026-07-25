//! Cross-shard glue for the geo `*STORE` family.
//!
//! `GEOSEARCHSTORE dst src …` reads one key and writes another, and so do
//! `GEORADIUS[BYMEMBER] src … STORE|STOREDIST dst`. The two keys hash to
//! different shards, and neither family puts them where the routing layer's
//! catch-all (`Route::Single(1)` — hash argv[1]) expects: GEOSEARCHSTORE has
//! the DESTINATION at argv[1] and the legacy forms have the SOURCE there. So
//! [`geo_store_route`] hands the runtime both keys, and [`geo_search`] is the
//! read half it calls back on the SOURCE's shard — the write then lands on the
//! DESTINATION's shard as a plain ZSet materialisation.
//!
//! Query-only forms (no STORE) route by their single key as before; the `_RO`
//! variants never store (they reject the option) and stay single-key too.

use kevy_resp::{Argv, ArgvView, CmdError, encode_error};
use kevy_rt::{GeoHits, Route};
use kevy_store::Store;

use crate::cmd::store_err;

use super::radius::{legacy_store_dst, plan_radius};
use super::search::{Opts, SearchError, plan_geosearchstore, search_pairs};

/// `Some(Route::GeoStore { .. })` for a geo command that writes a destination
/// key; `None` for every other shape (including the query-only geo forms) —
/// the caller keeps its normal single-key route for those.
pub(crate) fn geo_store_route<A: ArgvView + ?Sized>(verb: &[u8], args: &A) -> Option<Route> {
    match verb {
        b"GEOSEARCHSTORE" if args.len() >= 5 => Some(Route::GeoStore {
            dst: args[1].to_vec(),
            src: args[2].to_vec(),
        }),
        // Legacy prefix: verb key lon lat radius unit  → options from 6.
        b"GEORADIUS" if args.len() >= 6 => legacy_store_dst(args, 6).map(|dst| Route::GeoStore {
            src: args[1].to_vec(),
            dst,
        }),
        // Legacy prefix: verb key member radius unit   → options from 5.
        b"GEORADIUSBYMEMBER" if args.len() >= 5 => {
            legacy_store_dst(args, 5).map(|dst| Route::GeoStore { src: args[1].to_vec(), dst })
        }
        _ => None,
    }
}

/// Run a geo `*STORE`'s search against the SOURCE key (this shard owns it) and
/// return the `(member, score)` pairs the destination's shard will write.
/// Scores are final here — geohashes, or `STOREDIST` distances already scaled
/// into the unit the command asked for.
pub(crate) fn geo_search(store: &mut Store, argv: &[Vec<u8>]) -> GeoHits {
    let mut args = Argv::with_capacity(argv.len(), 0);
    for a in argv {
        args.push(a);
    }
    let planned = match verb_of(argv).as_slice() {
        b"GEOSEARCHSTORE" => plan_geosearchstore(&args),
        b"GEORADIUS" => plan_radius(&args, false).map(|(src, p)| (src, p.opts)),
        b"GEORADIUSBYMEMBER" => plan_radius(&args, true).map(|(src, p)| (src, p.opts)),
        // Only the three verbs above ever route to `Route::GeoStore`.
        _ => Err(CmdError::Wire("ERR unknown command")),
    };
    match planned {
        Ok((src, opts)) => run(store, &src, &opts),
        Err(msg) => GeoHits::Error(encoded(|out| encode_error(out, msg.as_wire()))),
    }
}

fn run(store: &mut Store, src: &[u8], opts: &Opts) -> GeoHits {
    match search_pairs(store, src, opts) {
        Ok(pairs) => GeoHits::Pairs(pairs),
        Err(SearchError::NoMember) => GeoHits::Error(encoded(|out| {
            encode_error(out, "ERR could not decode requested zset member");
        })),
        Err(SearchError::Store(e)) => GeoHits::Error(encoded(|out| store_err(out, e))),
    }
}

/// Uppercased verb of an owned argv, for the ≤16-byte geo verbs.
fn verb_of(argv: &[Vec<u8>]) -> Vec<u8> {
    argv.first().map(|v| v.to_ascii_uppercase()).unwrap_or_default()
}

fn encoded(f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::new();
    f(&mut out);
    out
}
