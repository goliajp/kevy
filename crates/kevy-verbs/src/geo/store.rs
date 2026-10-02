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

use kevy_resp::{Argv, ArgvView, CmdError};
use kevy_store::{Store, StoreError};

use super::radius::legacy_store_dst;
use super::search::{Form, GeoError, plan, search_pairs};

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

/// Why a storing geo command's search refused; [`Self::as_wire`] is the
/// error reply the command answers.
///
/// ```
/// use kevy_verbs::geo::StoreSearchError;
///
/// let e = StoreSearchError::NoMember;
/// assert_eq!(e.as_wire(), "ERR could not decode requested zset member");
/// assert_eq!(e.to_string(), "could not decode requested zset member");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StoreSearchError {
    /// The command's arguments are refused (syntax, arity, a bad value).
    ///
    /// ```
    /// use kevy_verbs::geo::{StoreSearchError, store_search};
    /// let argv = |s: &str| s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>();
    /// let mut store = kevy_store::Store::new();
    /// // a search with a centre but no BYRADIUS / BYBOX
    /// let r = store_search(&mut store, &argv("GEOSEARCHSTORE dst src FROMLONLAT 13 38"));
    /// assert!(matches!(r, Err(StoreSearchError::Refused(_))));
    /// ```
    Refused(CmdError),
    /// A `FROMMEMBER` / `BYMEMBER` member the source key does not hold.
    ///
    /// ```
    /// use kevy_verbs::geo::{StoreSearchError, store_search};
    /// let argv = |s: &str| s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>();
    /// let mut store = kevy_store::Store::new();
    /// let add = kevy_resp::Argv::from(argv("GEOADD src 13.361389 38.115556 Palermo"));
    /// kevy_verbs::exec(&mut store, b"GEOADD", &add, &mut Vec::new());
    /// let r = store_search(&mut store, &argv("GEOSEARCHSTORE dst src FROMMEMBER Rome BYRADIUS 10 km"));
    /// assert_eq!(r, Err(StoreSearchError::NoMember));
    /// ```
    NoMember,
    /// The source key refused (wrong type, out of memory).
    ///
    /// ```
    /// use kevy_verbs::geo::{StoreSearchError, store_search};
    /// let argv = |s: &str| s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>();
    /// let mut store = kevy_store::Store::new();
    /// kevy_verbs::exec(&mut store, b"SET", &kevy_resp::Argv::from(argv("SET src v")), &mut Vec::new());
    /// let r = store_search(&mut store, &argv("GEOSEARCHSTORE dst src FROMLONLAT 13 38 BYRADIUS 1 km"));
    /// assert_eq!(r, Err(StoreSearchError::Store(kevy_store::StoreError::WrongType)));
    /// ```
    Store(StoreError),
    /// A `FROMLONLAT` / GEORADIUS centre off the map; its reply quotes the
    /// pair, which [`StoreSearchError::to_wire`] carries.
    ///
    /// ```
    /// use kevy_verbs::geo::{StoreSearchError, store_search};
    /// let argv = |s: &str| s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>();
    /// let mut store = kevy_store::Store::new();
    /// let r = store_search(&mut store, &argv("GEOSEARCHSTORE dst src FROMLONLAT 200 38 BYRADIUS 1 km"));
    /// assert_eq!(r.unwrap_err().to_wire(), "ERR invalid longitude,latitude pair 200.000000,38.000000");
    /// ```
    BadCenter(BadCenter),
    /// A GEOSEARCHSTORE with no `FROMMEMBER` / `FROMLONLAT`, the verb as it
    /// was typed: Redis's reply names it so.
    ///
    /// ```
    /// use kevy_verbs::geo::{StoreSearchError, store_search};
    /// let argv = |s: &str| s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>();
    /// let r = store_search(&mut kevy_store::Store::new(), &argv("geosearchstore d s BYRADIUS 1 km COUNT 1"));
    /// assert_eq!(
    ///     r.unwrap_err().to_wire(),
    ///     "ERR exactly one of FROMMEMBER or FROMLONLAT can be specified for geosearchstore",
    /// );
    /// ```
    NoFrom([u8; 14]),
    /// A GEOSEARCHSTORE with no `BYRADIUS` / `BYBOX`, the verb as typed.
    ///
    /// ```
    /// use kevy_verbs::geo::{StoreSearchError, store_search};
    /// let argv = |s: &str| s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>();
    /// let r = store_search(&mut kevy_store::Store::new(), &argv("GEOSEARCHSTORE d s FROMLONLAT 1 2 COUNT 1"));
    /// assert!(matches!(r, Err(StoreSearchError::NoShape(_))));
    /// ```
    NoShape([u8; 14]),
}

/// A longitude / latitude pair off the map, as given.
///
/// ```
/// use kevy_verbs::geo::{StoreSearchError, store_search};
/// let argv = |s: &str| s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>();
/// let r = store_search(&mut kevy_store::Store::new(), &argv("GEORADIUS src 0 90 1 km STORE dst"));
/// assert!(matches!(r, Err(StoreSearchError::BadCenter(_))));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct BadCenter {
    lon: f64,
    lat: f64,
}

impl PartialEq for BadCenter {
    fn eq(&self, o: &Self) -> bool {
        self.lon.to_bits() == o.lon.to_bits() && self.lat.to_bits() == o.lat.to_bits()
    }
}

impl Eq for BadCenter {}

impl std::hash::Hash for BadCenter {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        (self.lon.to_bits(), self.lat.to_bits()).hash(h);
    }
}

impl StoreSearchError {
    /// The error reply the command answers, the pair quoted for a centre
    /// off the map.
    ///
    /// ```
    /// let e = kevy_verbs::geo::StoreSearchError::NoMember;
    /// assert_eq!(e.to_wire(), "ERR could not decode requested zset member");
    /// ```
    pub fn to_wire(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::BadCenter(c) => super::search::bad_pair(c.lon, c.lat).into(),
            Self::NoFrom(verb) => GeoError::NoFrom.text(verb),
            Self::NoShape(verb) => GeoError::NoShape.text(verb),
            _ => self.as_wire().into(),
        }
    }

    /// The error reply the command answers.
    ///
    /// ```
    /// let e = kevy_verbs::geo::StoreSearchError::Store(kevy_store::StoreError::WrongType);
    /// assert!(e.as_wire().starts_with("WRONGTYPE"));
    /// ```
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Refused(e) => e.as_wire(),
            Self::NoMember => "ERR could not decode requested zset member",
            Self::Store(e) => e.as_wire(),
            // the pair itself, and the verb, are in `to_wire`
            Self::BadCenter(_) => "ERR invalid longitude,latitude pair",
            Self::NoFrom(_) => "ERR exactly one of FROMMEMBER or FROMLONLAT can be specified",
            Self::NoShape(_) => "ERR exactly one of BYRADIUS and BYBOX can be specified",
        }
    }
}

impl std::fmt::Display for StoreSearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            // the wire text without its error code
            _ => {
                let wire = self.to_wire();
                f.write_str(wire.split_once(' ').map_or(&*wire, |(_, text)| text))
            }
        }
    }
}

impl std::error::Error for StoreSearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Refused(e) => Some(e),
            Self::Store(e) => Some(e),
            Self::NoMember | Self::BadCenter(_) | Self::NoFrom(_) | Self::NoShape(_) => None,
        }
    }
}

/// Run a storing geo command's search against its source key in `store`:
/// the `(member, score)` pairs the destination is replaced with (none =
/// the destination is deleted), or why the command refuses.
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
pub fn store_search(
    store: &mut Store,
    argv: &[Vec<u8>],
) -> Result<Vec<(Vec<u8>, f64)>, StoreSearchError> {
    let mut args = Argv::with_capacity(argv.len(), 0);
    for a in argv {
        args.push(a);
    }
    let form = match verb_of(argv).as_slice() {
        b"GEOSEARCHSTORE" => Form::SearchStore,
        b"GEORADIUS" => Form::Radius { read_only: false },
        b"GEORADIUSBYMEMBER" => Form::ByMember { read_only: false },
        // only the three verbs above store
        _ => return Err(StoreSearchError::Refused(CmdError::Wire("ERR unknown command"))),
    };
    let typed = || {
        let mut verb = [0u8; 14];
        let given = argv.first().map_or(&[][..], Vec::as_slice);
        let n = given.len().min(14);
        verb[..n].copy_from_slice(&given[..n]);
        verb
    };
    let q = plan(store, &args, form).map_err(|e| match e {
        GeoError::Wire(s) => StoreSearchError::Refused(CmdError::Wire(s)),
        GeoError::BadCenter(lon, lat) => StoreSearchError::BadCenter(BadCenter { lon, lat }),
        GeoError::NoMember => StoreSearchError::NoMember,
        GeoError::Store(e) => StoreSearchError::Store(e),
        GeoError::NoFrom => StoreSearchError::NoFrom(typed()),
        GeoError::NoShape => StoreSearchError::NoShape(typed()),
    })?;
    search_pairs(store, &q).map_err(StoreSearchError::Store)
}

/// Uppercased verb of an owned argv, for the ≤16-byte geo verbs.
fn verb_of(argv: &[Vec<u8>]) -> Vec<u8> {
    argv.first().map(|v| v.to_ascii_uppercase()).unwrap_or_default()
}
