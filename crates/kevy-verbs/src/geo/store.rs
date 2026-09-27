//! The two halves of a geo command that stores its result.
//!
//! `GEOSEARCHSTORE dst src …` reads one key and writes another, and so do
//! `GEORADIUS[BYMEMBER] src … STORE|STOREDIST dst`. A caller whose keys
//! live on more than one keyspace needs both keys before it runs anything
//! ([`store_keys`]), and the search on the source's keyspace on its own
//! ([`store_search`]); the write is then a plain sorted-set replacement on
//! the destination's keyspace. Neither family puts the keys where a
//! "first argument is the key" rule expects: GEOSEARCHSTORE has the
//! destination first, the legacy forms have the source first and the
//! destination in the option tail.
//!
//! Query-only forms (no STORE) have one key, and the `_RO` variants never
//! store (they refuse the option).

use kevy_resp::{Argv, ArgvView, CmdError, encode_error};
use kevy_store::Store;

use crate::reply::store_err;

use super::radius::{legacy_store_dst, plan_radius};
use super::search::{Opts, SearchError, plan_geosearchstore, search_pairs};

/// `(source, destination)` of a geo command that writes a destination
/// key; `None` for every other shape, including the query-only forms.
/// `verb` is uppercase.
///
/// ```
/// let argv = kevy_resp::Argv::from(
///     "GEORADIUS src 13 38 10 km STORE dst".split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>(),
/// );
/// let (src, dst) = kevy_verbs::geo::store_keys(b"GEORADIUS", &argv).unwrap();
/// assert_eq!((&src[..], &dst[..]), (&b"src"[..], &b"dst"[..]));
/// let query = kevy_resp::Argv::from(
///     "GEORADIUS src 13 38 10 km".split(' ').map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>(),
/// );
/// assert!(kevy_verbs::geo::store_keys(b"GEORADIUS", &query).is_none());
/// ```
pub fn store_keys<A: ArgvView + ?Sized>(verb: &[u8], args: &A) -> Option<(Vec<u8>, Vec<u8>)> {
    match verb {
        b"GEOSEARCHSTORE" if args.len() >= 5 => Some((args[2].to_vec(), args[1].to_vec())),
        // legacy prefix: verb key lon lat radius unit, options from 6
        b"GEORADIUS" if args.len() >= 6 => {
            legacy_store_dst(args, 6).map(|dst| (args[1].to_vec(), dst))
        }
        // legacy prefix: verb key member radius unit, options from 5
        b"GEORADIUSBYMEMBER" if args.len() >= 5 => {
            legacy_store_dst(args, 5).map(|dst| (args[1].to_vec(), dst))
        }
        _ => None,
    }
}

/// Run a storing geo command's search against its source key in `store`:
/// the `(member, score)` pairs the destination is replaced with (none =
/// the destination is deleted), or the error reply the command answers.
/// Scores are final — geohashes, or `STOREDIST` distances in the unit
/// the command asked for.
///
/// ```
/// let mut store = kevy_store::Store::new();
/// let argv = |s: &str| s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>();
/// let add = kevy_resp::Argv::from(argv("GEOADD src 13.361389 38.115556 Palermo"));
/// kevy_verbs::exec(&mut store, b"GEOADD", &add, &mut Vec::new());
/// let hits = kevy_verbs::geo::store_search(
///     &mut store,
///     &argv("GEOSEARCHSTORE dst src FROMLONLAT 13 38 BYRADIUS 200 km"),
/// );
/// assert_eq!(hits.unwrap()[0].0, b"Palermo");
/// ```
pub fn store_search(store: &mut Store, argv: &[Vec<u8>]) -> Result<Vec<(Vec<u8>, f64)>, Vec<u8>> {
    let mut args = Argv::with_capacity(argv.len(), 0);
    for a in argv {
        args.push(a);
    }
    let planned = match verb_of(argv).as_slice() {
        b"GEOSEARCHSTORE" => plan_geosearchstore(&args),
        b"GEORADIUS" => plan_radius(&args, false).map(|(src, p)| (src, p.opts)),
        b"GEORADIUSBYMEMBER" => plan_radius(&args, true).map(|(src, p)| (src, p.opts)),
        // only the three verbs above store
        _ => Err(CmdError::Wire("ERR unknown command")),
    };
    match planned {
        Ok((src, opts)) => run(store, &src, &opts),
        Err(msg) => Err(encoded(|out| encode_error(out, msg.as_wire()))),
    }
}

fn run(store: &mut Store, src: &[u8], opts: &Opts) -> Result<Vec<(Vec<u8>, f64)>, Vec<u8>> {
    search_pairs(store, src, opts).map_err(|e| match e {
        SearchError::NoMember => {
            encoded(|out| encode_error(out, "ERR could not decode requested zset member"))
        }
        SearchError::Store(e) => encoded(|out| store_err(out, e)),
    })
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
