//! `SET` with Redis's whole option set, and `GETEX`, which shares its
//! expiry options. The forms the hot path sees — bare, `NX`/`XX`,
//! `EX`/`PX` — go to the store in one call; the rest read the old value
//! first.

use std::time::Duration;

use kevy_resp::{ArgvView, encode_bulk, encode_error, encode_null_bulk, encode_simple_string};
use kevy_store::{SetCondition, Store};

use crate::args::{arg_i64, upper_verb};
use crate::reply::{ERR_NOT_INT, ERR_SYNTAX, store_err, wrong_args};
use crate::{Effect, changed};

/// When the write happens.
#[derive(Clone, Copy)]
enum Cond<'a> {
    Always,
    Nx,
    Xx,
    Eq(&'a [u8]),
    Ne(&'a [u8]),
    DigestEq(&'a [u8]),
    DigestNe(&'a [u8]),
}

/// What becomes of the deadline.
#[derive(Clone, Copy)]
enum Ttl<'a> {
    Clear,
    Keep,
    Persist,
    Ex(&'a [u8]),
    Px(&'a [u8]),
    ExAt(&'a [u8]),
    PxAt(&'a [u8]),
}

/// One option word, and how many arguments it takes up.
enum Word<'a> {
    Cond(Cond<'a>),
    Ttl(Ttl<'a>),
    Get,
}

struct Opts<'a> {
    cond: Cond<'a>,
    get: bool,
    ttl: Ttl<'a>,
}

/// A deadline the options asked for: milliseconds from now, or a unix
/// time in milliseconds.
#[derive(Clone, Copy)]
enum Deadline {
    In(u64),
    At(u64),
}

/// The option at `args[i]`, with its width; `None` is a syntax error. `SET`
/// takes the conditions, `GET` and `KEEPTTL`; `GETEX` takes `PERSIST`.
fn word<A: ArgvView + ?Sized>(args: &A, i: usize, set: bool) -> Option<(Word<'_>, usize)> {
    let mut buf = [0u8; 32];
    let flag = |w| Some((w, 1));
    let valued = |f: fn(&[u8]) -> Word<'_>| args.get(i + 1).map(|v| (f(v), 2));
    match upper_verb(&args[i], &mut buf) {
        b"NX" if set => flag(Word::Cond(Cond::Nx)),
        b"XX" if set => flag(Word::Cond(Cond::Xx)),
        b"GET" if set => flag(Word::Get),
        b"KEEPTTL" if set => flag(Word::Ttl(Ttl::Keep)),
        b"PERSIST" if !set => flag(Word::Ttl(Ttl::Persist)),
        b"IFEQ" if set => valued(|v| Word::Cond(Cond::Eq(v))),
        b"IFNE" if set => valued(|v| Word::Cond(Cond::Ne(v))),
        b"IFDEQ" if set => valued(|v| Word::Cond(Cond::DigestEq(v))),
        b"IFDNE" if set => valued(|v| Word::Cond(Cond::DigestNe(v))),
        b"EX" => valued(|v| Word::Ttl(Ttl::Ex(v))),
        b"PX" => valued(|v| Word::Ttl(Ttl::Px(v))),
        b"EXAT" => valued(|v| Word::Ttl(Ttl::ExAt(v))),
        b"PXAT" => valued(|v| Word::Ttl(Ttl::PxAt(v))),
        _ => None,
    }
}

/// `new` may replace `cur` when nothing of its group was given yet, or the
/// same option was: a repeat is allowed and the last one wins.
fn fits<T>(cur: &T, unset: &T, new: &T) -> bool {
    let d = core::mem::discriminant;
    d(cur) == d(unset) || d(cur) == d(new)
}

fn parse<A: ArgvView + ?Sized>(args: &A, from: usize, set: bool) -> Option<Opts<'_>> {
    let mut o = Opts { cond: Cond::Always, get: false, ttl: Ttl::Clear };
    let mut i = from;
    while i < args.len() {
        let (w, width) = word(args, i, set)?;
        match w {
            Word::Cond(c) if fits(&o.cond, &Cond::Always, &c) => o.cond = c,
            Word::Ttl(t) if fits(&o.ttl, &Ttl::Clear, &t) => o.ttl = t,
            Word::Get => o.get = true,
            _ => return None,
        }
        i += width;
    }
    Some(o)
}

/// The deadline `ttl` names, checked as Redis checks it: a positive
/// number whose milliseconds, from now for the relative forms, still fit
/// an `i64`.
fn deadline(ttl: Ttl<'_>, cmd: &str) -> Result<Option<Deadline>, String> {
    let (raw, secs, relative) = match ttl {
        Ttl::Ex(v) => (v, true, true),
        Ttl::Px(v) => (v, false, true),
        Ttl::ExAt(v) => (v, true, false),
        Ttl::PxAt(v) => (v, false, false),
        Ttl::Clear | Ttl::Keep | Ttl::Persist => return Ok(None),
    };
    let n = arg_i64(raw).ok_or_else(|| ERR_NOT_INT.to_string())?;
    let invalid = || format!("ERR invalid expire time in '{cmd}' command");
    if n <= 0 || (secs && n > i64::MAX / 1000) {
        return Err(invalid());
    }
    let ms = if secs { n * 1000 } else { n };
    // the clock is read only when the sum could overflow: a unix time in
    // milliseconds stays below 2^42 for the next century
    if relative && ms > i64::MAX - (1 << 42) && ms.checked_add(now_ms() as i64).is_none() {
        return Err(invalid());
    }
    Ok(Some(if relative { Deadline::In(ms as u64) } else { Deadline::At(ms as u64) }))
}

fn now_ms() -> u64 {
    kevy_store::now_unix_ms()
}

/// `SET key value [NX | XX | IFEQ v | IFNE v | IFDEQ d | IFDNE d] [GET]
/// [EX s | PX ms | EXAT ts | PXAT ms-ts | KEEPTTL]`.
///
/// ```
/// use kevy_verbs::Effect;
/// let mut store = kevy_store::Store::new();
/// let argv = kevy_resp::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec(), b"NX".to_vec()]);
/// let mut out = Vec::new();
/// assert_eq!(kevy_verbs::cmd::set(&mut store, &argv, &mut out), Effect::Write);
/// assert_eq!(kevy_verbs::cmd::set(&mut store, &argv, &mut out), Effect::Unchanged);
/// assert_eq!(out, b"+OK\r\n$-1\r\n");
/// ```
pub fn set<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) -> Effect {
    if args.len() < 3 {
        wrong_args(out, "set");
        return Effect::Unchanged;
    }
    let Some(o) = parse(args, 3, true) else {
        encode_error(out, ERR_SYNTAX);
        return Effect::Unchanged;
    };
    let dl = match deadline(o.ttl, "set") {
        Ok(d) => d,
        Err(e) => {
            encode_error(out, &e);
            return Effect::Unchanged;
        }
    };
    let cond = match o.cond {
        Cond::Always => SetCondition::Always,
        Cond::Nx => SetCondition::IfAbsent,
        Cond::Xx => SetCondition::IfPresent,
        _ => return set_reading(store, args, &o, dl, out),
    };
    let expire = match (o.ttl, dl) {
        (Ttl::Clear, None) => None,
        (_, Some(Deadline::In(ms))) => Some(Duration::from_millis(ms)),
        _ => return set_reading(store, args, &o, dl, out),
    };
    if o.get {
        return set_reading(store, args, &o, dl, out);
    }
    let done = store.set_slice(&args[1], &args[2], expire, cond);
    if done {
        encode_simple_string(out, "OK");
    } else {
        encode_null_bulk(out); // the condition was not met
    }
    changed(done)
}

/// The `SET` forms that look at the key first: `GET`, the value and
/// digest conditions, `KEEPTTL` and the absolute deadlines.
fn set_reading<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    o: &Opts<'_>,
    dl: Option<Deadline>,
    out: &mut Vec<u8>,
) -> Effect {
    let key = &args[1];
    // GET and the value conditions read a string; anything else is refused
    let reads = o.get || !matches!(o.cond, Cond::Always | Cond::Nx | Cond::Xx);
    let old = if reads {
        match store.get(key) {
            Ok(v) => v.map(|v| v.into_owned()),
            Err(e) => {
                store_err(out, e);
                return Effect::Unchanged;
            }
        }
    } else {
        None
    };
    let present = if reads { old.is_some() } else { store.key_exists(key) };
    let met = match met(o.cond, present, old.as_deref()) {
        Ok(m) => m,
        Err(e) => {
            encode_error(out, e);
            return Effect::Unchanged;
        }
    };
    let reply = |out: &mut Vec<u8>, wrote: bool| match (&old, o.get) {
        (Some(v), true) => encode_bulk(out, v),
        (None, true) => encode_null_bulk(out),
        (_, false) if wrote => encode_simple_string(out, "OK"),
        _ => encode_null_bulk(out),
    };
    if !met {
        reply(out, false);
        return Effect::Unchanged;
    }
    let wrote = write(store, key, &args[2], o.ttl, dl, present);
    reply(out, true);
    changed(wrote)
}

/// Whether the condition lets the write through. A digest that is not 16
/// characters long is refused, but only once there is a value to compare.
fn met(cond: Cond<'_>, present: bool, old: Option<&[u8]>) -> Result<bool, &'static str> {
    let digest_is = |d: &[u8]| {
        if d.len() != 16 {
            return Err("ERR must be exactly 16 hexadecimal characters");
        }
        Ok(old.is_some_and(|v| crate::digest::hex(v).eq_ignore_ascii_case(d)))
    };
    Ok(match cond {
        Cond::Always => true,
        Cond::Nx => !present,
        Cond::Xx => present,
        Cond::Eq(v) => old == Some(v),
        Cond::Ne(v) => old != Some(v),
        Cond::DigestEq(d) if present => digest_is(d)?,
        Cond::DigestNe(d) if present => !digest_is(d)?,
        Cond::DigestEq(_) => false,
        Cond::DigestNe(_) => true,
    })
}

/// Store the value with the deadline asked for. A deadline already past
/// deletes the key instead, as Redis does; `false` when that left nothing
/// changed.
fn write(
    store: &mut Store,
    key: &[u8],
    value: &[u8],
    ttl: Ttl<'_>,
    dl: Option<Deadline>,
    present: bool,
) -> bool {
    match (ttl, dl) {
        (_, Some(Deadline::At(at))) if at <= now_ms() => store.del(&[key]) == 1,
        (_, Some(Deadline::At(at))) => {
            store.set_slice(key, value, None, SetCondition::Always);
            store.expire_at_unix_ms(key, at)
        }
        (_, Some(Deadline::In(ms))) => {
            store.set_slice(key, value, Some(Duration::from_millis(ms)), SetCondition::Always)
        }
        (Ttl::Keep, None) if present => {
            store.set_slice_keep_ttl(key, value);
            true
        }
        _ => store.set_slice(key, value, None, SetCondition::Always),
    }
}

/// `GETEX key [EX s | PX ms | EXAT ts | PXAT ms-ts | PERSIST]`: read, and
/// move the deadline in the same call. A missing key answers nil before
/// its options' numbers are read.
pub(crate) fn getex<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 2 {
        wrong_args(out, "getex");
        return Effect::Unchanged;
    }
    let Some(o) = parse(args, 2, false) else {
        encode_error(out, ERR_SYNTAX);
        return Effect::Unchanged;
    };
    let key = &args[1];
    let value = match store.get(key) {
        Ok(Some(v)) => v.into_owned(),
        Ok(None) => {
            encode_null_bulk(out);
            return Effect::Read;
        }
        Err(e) => {
            store_err(out, e);
            return Effect::Unchanged;
        }
    };
    let dl = match deadline(o.ttl, "getex") {
        Ok(d) => d,
        Err(e) => {
            encode_error(out, &e);
            return Effect::Unchanged;
        }
    };
    encode_bulk(out, &value);
    match dl {
        Some(Deadline::At(at)) => changed(store.expire_at_unix_ms(key, at)),
        Some(Deadline::In(ms)) => changed(store.expire(key, Duration::from_millis(ms))),
        None if matches!(o.ttl, Ttl::Persist) => changed(store.persist(key)),
        None => Effect::Read,
    }
}
