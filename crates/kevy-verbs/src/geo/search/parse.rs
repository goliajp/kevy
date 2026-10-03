//! Reading a geo search's arguments, in the order Redis reads them: the
//! source key's type first, then the fixed prefix of the legacy forms
//! (whose member is looked up there and then), then the options left to
//! right, each refused as soon as it is read, and last the checks that
//! need all of them.

use kevy_resp::{ArgvView, RespVersion, encode_error};
use kevy_store::{Store, StoreError};

use crate::args::{arg_f64, arg_i64, upper_verb};
use crate::reply::ERR_NOT_INT;

use super::super::{parse_unit, score_to_point};
use super::{Opts, Shape, Sort};

/// Why a geo search's arguments were refused.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::geo) enum GeoError {
    Wire(&'static str),
    /// A centre off the map; the reply quotes the pair.
    BadCenter(f64, f64),
    /// A `FROMMEMBER` / `BYMEMBER` member the source key does not hold.
    NoMember,
    Store(StoreError),
    /// A GEOSEARCH with no `FROMMEMBER` / `FROMLONLAT`; the reply names
    /// the verb as it was typed.
    NoFrom,
    /// A GEOSEARCH with no `BYRADIUS` / `BYBOX`.
    NoShape,
}

impl From<&'static str> for GeoError {
    fn from(s: &'static str) -> Self {
        Self::Wire(s)
    }
}

impl From<StoreError> for GeoError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl GeoError {
    /// The error reply; `verb` is the command's name as it was typed.
    pub(in crate::geo) fn emit(self, verb: &[u8], out: &mut Vec<u8>) {
        encode_error(out, &self.text(verb));
    }

    pub(in crate::geo) fn text(self, verb: &[u8]) -> std::borrow::Cow<'static, str> {
        let verb = String::from_utf8_lossy(verb);
        match self {
            Self::Wire(s) => s.into(),
            Self::BadCenter(lon, lat) => bad_pair(lon, lat).into(),
            Self::NoMember => "ERR could not decode requested zset member".into(),
            Self::Store(e) => e.as_wire().into(),
            Self::NoFrom => {
                format!("ERR exactly one of FROMMEMBER or FROMLONLAT can be specified for {verb}")
                    .into()
            }
            Self::NoShape => {
                format!("ERR exactly one of BYRADIUS and BYBOX can be specified for {verb}").into()
            }
        }
    }
}

/// Redis's reply to a longitude / latitude pair off the map.
pub(in crate::geo) fn bad_pair(lon: f64, lat: f64) -> String {
    format!("ERR invalid longitude,latitude pair {lon:.6},{lat:.6}")
}

/// Which command the arguments belong to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::geo) enum Form {
    /// `GEORADIUS[_RO] key lon lat radius unit …`
    Radius { read_only: bool },
    /// `GEORADIUSBYMEMBER[_RO] key member radius unit …`
    ByMember { read_only: bool },
    /// `GEOSEARCH key …`
    Search,
    /// `GEOSEARCHSTORE dst key …`
    SearchStore,
}

impl Form {
    /// The source key's index, where the options start, the least
    /// argument count, and the name the arity error gives.
    fn layout(self) -> (usize, usize, usize, &'static str) {
        match self {
            Form::Radius { .. } => {
                (1, 6, 6, "ERR wrong number of arguments for 'georadius' command")
            }
            Form::ByMember { .. } => {
                (1, 5, 5, "ERR wrong number of arguments for 'georadiusbymember' command")
            }
            Form::Search => (1, 2, 7, "ERR wrong number of arguments for 'geosearch' command"),
            Form::SearchStore => {
                (2, 3, 8, "ERR wrong number of arguments for 'geosearchstore' command")
            }
        }
    }

    fn can_store(self) -> bool {
        matches!(self, Form::Radius { read_only: false } | Form::ByMember { read_only: false })
    }
}

/// A geo search, read against its source key.
pub(in crate::geo) struct Query<'a> {
    pub(in crate::geo) src: &'a [u8],
    pub(in crate::geo) opts: Opts,
    /// The destination of a legacy `STORE` / `STOREDIST`, or of
    /// GEOSEARCHSTORE.
    pub(in crate::geo) store_dst: Option<&'a [u8]>,
    /// The source key does not exist: the search finds nothing, and its
    /// centre was never looked up.
    pub(in crate::geo) src_missing: bool,
}

/// What the options set so far.
#[derive(Default)]
struct Seen<'a> {
    center: Option<(f64, f64)>,
    from_member: bool,
    from_lonlat: bool,
    shape: Option<(Shape, f64)>,
    radius: bool,
    by_box: bool,
    sort: Sort,
    count: Option<usize>,
    any: bool,
    with_coord: bool,
    with_dist: bool,
    with_hash: bool,
    storedist: bool,
    store_dst: Option<&'a [u8]>,
}

/// Read a geo search's arguments against `store`.
pub(in crate::geo) fn plan<'a, A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &'a A,
    form: Form,
) -> Result<Query<'a>, GeoError> {
    let (src_at, base, least, arity) = form.layout();
    if args.len() < least {
        return Err(GeoError::Wire(arity));
    }
    let src = &args[src_at];
    // a missing key is an empty sorted set; anything else is refused first
    let src_missing = store.zcard(src)? == 0;
    let mut seen = Seen::default();
    match form {
        Form::Radius { .. } => {
            seen.center = Some(center(&args[2], &args[3])?);
            seen.shape = Some(radius(&args[4], &args[5])?);
        }
        Form::ByMember { .. } if !src_missing => {
            let point = score_to_point(store, src, &args[2])?.ok_or(GeoError::NoMember)?;
            seen.center = Some(point);
            seen.shape = Some(radius(&args[3], &args[4])?);
        }
        Form::ByMember { .. } | Form::Search => {}
        Form::SearchStore => seen.store_dst = Some(&args[1]),
    }
    let mut i = base;
    while i < args.len() {
        i += option(store, args, i, form, src_missing, &mut seen)?;
    }
    finish(args, form, src_missing, seen)
}

/// Read the option at `args[i]`; how many arguments it took.
fn option<'a, A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &'a A,
    i: usize,
    form: Form,
    src_missing: bool,
    s: &mut Seen<'a>,
) -> Result<usize, GeoError> {
    let mut buf = [0u8; 32];
    let word = upper_verb(&args[i], &mut buf);
    match word {
        b"WITHDIST" => s.with_dist = true,
        b"WITHHASH" => s.with_hash = true,
        b"WITHCOORD" => s.with_coord = true,
        b"ANY" => s.any = true,
        b"ASC" => s.sort = Sort::Asc,
        b"DESC" => s.sort = Sort::Desc,
        b"STOREDIST" if form == Form::SearchStore => s.storedist = true,
        _ => {
            let at = Valued { store, args, i, form, src_missing };
            return at.read(word, s)?.ok_or(GeoError::Wire("ERR syntax error"));
        }
    }
    Ok(1)
}

/// An option that takes values, at `args[i]`.
struct Valued<'s, 'a, A: ?Sized> {
    store: &'s mut Store,
    args: &'a A,
    i: usize,
    form: Form,
    src_missing: bool,
}

impl<'a, A: ArgvView + ?Sized> Valued<'_, 'a, A> {
    /// How many arguments the option took; `None` when `word` is no option
    /// here, or its values are missing.
    fn read(self, word: &[u8], s: &mut Seen<'a>) -> Result<Option<usize>, GeoError> {
        let (args, i) = (self.args, self.i);
        let left = args.len() - i - 1;
        let search = matches!(self.form, Form::Search | Form::SearchStore);
        Ok(Some(match word {
            b"COUNT" if left >= 1 => {
                let n = arg_i64(&args[i + 1]).ok_or(GeoError::Wire(ERR_NOT_INT))?;
                if n <= 0 {
                    return Err(GeoError::Wire("ERR COUNT must be > 0"));
                }
                s.count = Some(n as usize);
                2
            }
            b"STORE" | b"STOREDIST" if left >= 1 && self.form.can_store() => {
                s.store_dst = Some(&args[i + 1]);
                s.storedist = word == b"STOREDIST";
                2
            }
            b"FROMMEMBER" if left >= 1 && search && !s.from_lonlat => {
                if !self.src_missing {
                    let src = &args[self.form.layout().0];
                    let point = score_to_point(self.store, src, &args[i + 1])?;
                    s.center = Some(point.ok_or(GeoError::NoMember)?);
                }
                s.from_member = true;
                2
            }
            b"FROMLONLAT" if left >= 2 && search && !s.from_member => {
                s.center = Some(center(&args[i + 1], &args[i + 2])?);
                s.from_lonlat = true;
                3
            }
            b"BYRADIUS" if left >= 2 && search && !s.by_box => {
                s.shape = Some(radius(&args[i + 1], &args[i + 2])?);
                s.radius = true;
                3
            }
            b"BYBOX" if left >= 3 && search && !s.radius => {
                s.shape = Some(boxed(&args[i + 1], &args[i + 2], &args[i + 3])?);
                s.by_box = true;
                4
            }
            _ => return Ok(None),
        }))
    }
}

/// The checks that need every option.
fn finish<'a, A: ArgvView + ?Sized>(
    args: &'a A,
    form: Form,
    src_missing: bool,
    s: Seen<'a>,
) -> Result<Query<'a>, GeoError> {
    if s.store_dst.is_some() && (s.with_dist || s.with_hash || s.with_coord) {
        return Err(GeoError::Wire(if form == Form::SearchStore {
            "ERR GEOSEARCHSTORE is not compatible with WITHDIST, WITHHASH and WITHCOORD options"
        } else {
            "ERR STORE option in GEORADIUS is not compatible with WITHDIST, WITHHASH and WITHCOORD options"
        }));
    }
    let search = matches!(form, Form::Search | Form::SearchStore);
    if search && !(s.from_member || s.from_lonlat) {
        return Err(GeoError::NoFrom);
    }
    if search && !(s.radius || s.by_box) {
        return Err(GeoError::NoShape);
    }
    if s.any && s.count.is_none() {
        return Err(GeoError::Wire("ERR the ANY argument requires COUNT argument"));
    }
    // a member looked up on a missing key has no centre; nothing is searched
    let (shape, unit) = s.shape.unwrap_or((Shape::Radius { r_m: 0.0 }, 1.0));
    let src_at = form.layout().0;
    Ok(Query {
        src: &args[src_at],
        opts: Opts {
            center: s.center.unwrap_or((0.0, 0.0)),
            shape,
            unit,
            sort: s.sort,
            count: s.count,
            any: s.any,
            with_coord: s.with_coord,
            with_dist: s.with_dist,
            with_hash: s.with_hash,
            storedist: s.storedist,
            proto: RespVersion::V2,
        },
        store_dst: s.store_dst,
        src_missing,
    })
}

/// A centre's longitude and latitude, checked against the map.
fn center(lon: &[u8], lat: &[u8]) -> Result<(f64, f64), GeoError> {
    let lon = arg_f64(lon).ok_or("ERR value is not a valid float")?;
    let lat = arg_f64(lat).ok_or("ERR value is not a valid float")?;
    if kevy_geo::encode_score(lon, lat).is_none() {
        return Err(GeoError::BadCenter(lon, lat));
    }
    Ok((lon, lat))
}

fn unit(b: &[u8]) -> Result<f64, GeoError> {
    parse_unit(b).ok_or(GeoError::Wire("ERR unsupported unit provided. please use M, KM, FT, MI"))
}

/// `radius unit`, the radius in metres and the unit's metres.
fn radius(r: &[u8], u: &[u8]) -> Result<(Shape, f64), GeoError> {
    let r = arg_f64(r).ok_or("ERR need numeric radius")?;
    if r < 0.0 {
        return Err(GeoError::Wire("ERR radius cannot be negative"));
    }
    let u = unit(u)?;
    Ok((Shape::Radius { r_m: r * u }, u))
}

/// `width height unit`.
fn boxed(w: &[u8], h: &[u8], u: &[u8]) -> Result<(Shape, f64), GeoError> {
    let w = arg_f64(w).ok_or("ERR need numeric width")?;
    let h = arg_f64(h).ok_or("ERR need numeric height")?;
    if w < 0.0 || h < 0.0 {
        return Err(GeoError::Wire("ERR height or width cannot be negative"));
    }
    let u = unit(u)?;
    Ok((Shape::Box { w_m: w * u, h_m: h * u }, u))
}
