//! The pops that try several keys in order and take from the first one
//! holding something — `ZMPOP`, `LMPOP` and their blocking forms `BZMPOP`,
//! `BLMPOP` — and the grammar they share. Every key named must live in the
//! store passed in: spreading them across shards is the caller's job.

use kevy_resp::{ArgvView, CmdError, RespVersion, encode_array_len, encode_bulk, encode_error};
use kevy_store::{ListEnd, Store, StoreError};

use crate::Effect;
use crate::args::arg_i64;
use crate::list_move::timeout_refusal;
use crate::reply::{store_err, wrong_args};
use crate::zset_pick::{score, zrem_record};

/// One command of this group over RESP2; `None` = not in the group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    let v2 = RespVersion::V2;
    Some(match cmd {
        b"ZMPOP" => zmpop(store, args, out, v2),
        b"LMPOP" => lmpop(store, args, out, v2),
        b"BZMPOP" => bzmpop(store, args, out, v2),
        b"BLMPOP" => blmpop(store, args, out, v2),
        _ => return None,
    })
}

/// A parsed `numkeys key… end [COUNT n]` tail: the keys are the
/// `numkeys` arguments after the count.
///
/// ```
/// use kevy_resp::Argv;
/// use kevy_store::ListEnd;
/// let a = |v: &[&str]| Argv::from(v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>());
/// let p = kevy_verbs::mpop::parse_zmpop(&a(&["ZMPOP", "2", "a", "b", "MAX", "COUNT", "3"]), 1)?;
/// assert_eq!((p.numkeys, p.end, p.count), (2, ListEnd::Right, 3));
/// # Ok::<(), kevy_resp::CmdError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MPopArgs {
    /// How many keys follow the count.
    pub numkeys: usize,
    /// Which end to take from. A sorted set's left end is its lowest
    /// score, so `MIN` is [`ListEnd::Left`] and `MAX` [`ListEnd::Right`].
    pub end: ListEnd,
    /// How many to take from the key that has some (1 without `COUNT`).
    pub count: usize,
}

/// `[B]ZMPOP`'s tail, with the key count at argument `at`.
pub fn parse_zmpop<A: ArgvView + ?Sized>(args: &A, at: usize) -> Result<MPopArgs, CmdError> {
    parse(args, at, [b"MIN", b"MAX"])
}

/// `[B]LMPOP`'s tail, with the key count at argument `at`.
pub fn parse_lmpop<A: ArgvView + ?Sized>(args: &A, at: usize) -> Result<MPopArgs, CmdError> {
    parse(args, at, [b"LEFT", b"RIGHT"])
}

fn parse<A: ArgvView + ?Sized>(
    args: &A,
    at: usize,
    ends: [&[u8]; 2],
) -> Result<MPopArgs, CmdError> {
    let numkeys = match arg_i64(&args[at]) {
        Some(n) if n > 0 => n as usize,
        _ => return Err(CmdError::Wire("ERR numkeys should be greater than 0")),
    };
    let end_at = at.saturating_add(1).saturating_add(numkeys);
    let end = match args.get(end_at) {
        Some(e) if e.eq_ignore_ascii_case(ends[0]) => ListEnd::Left,
        Some(e) if e.eq_ignore_ascii_case(ends[1]) => ListEnd::Right,
        _ => return Err(CmdError::Wire("ERR syntax error")),
    };
    let count = match args.len() - end_at - 1 {
        0 => 1,
        2 if args[end_at + 1].eq_ignore_ascii_case(b"COUNT") => match arg_i64(&args[end_at + 2]) {
            Some(c) if c > 0 => c as usize,
            _ => return Err(CmdError::Wire("ERR count should be greater than 0")),
        },
        _ => return Err(CmdError::Wire("ERR syntax error")),
    };
    Ok(MPopArgs { numkeys, end, count })
}

/// What a pop took from one key.
enum Taken {
    Members(Vec<(Vec<u8>, f64)>),
    Elements(Vec<Vec<u8>>),
}

impl Taken {
    fn is_empty(&self) -> bool {
        match self {
            Taken::Members(m) => m.is_empty(),
            Taken::Elements(e) => e.is_empty(),
        }
    }
}

fn take(store: &mut Store, key: &[u8], zset: bool, p: MPopArgs) -> Result<Taken, StoreError> {
    Ok(match (zset, p.end) {
        (true, ListEnd::Left) => Taken::Members(store.zpopmin(key, p.count)?),
        (true, ListEnd::Right) => Taken::Members(store.zpopmax(key, p.count)?),
        (false, ListEnd::Left) => Taken::Elements(store.lpop(key, p.count)?),
        (false, ListEnd::Right) => Taken::Elements(store.rpop(key, p.count)?),
    })
}

/// `[key, [taken…]]`, and the single-key pop that records it.
fn emit(out: &mut Vec<u8>, key: &[u8], taken: &Taken, p: MPopArgs, proto: RespVersion) -> Effect {
    encode_array_len(out, 2);
    encode_bulk(out, key);
    match taken {
        Taken::Members(members) => {
            encode_array_len(out, members.len() as i64);
            for (m, s) in members {
                encode_array_len(out, 2);
                encode_bulk(out, m);
                score(out, *s, proto);
            }
            zrem_record(key, members)
        }
        Taken::Elements(elements) => {
            encode_array_len(out, elements.len() as i64);
            for e in elements {
                encode_bulk(out, e);
            }
            let verb: &[u8] = if p.end == ListEnd::Left { b"LPOP" } else { b"RPOP" };
            let n = elements.len().to_string().into_bytes();
            Effect::Record(vec![verb.to_vec(), key.to_vec(), n])
        }
    }
}

/// `ZMPOP numkeys key… MIN|MAX [COUNT n]`.
pub fn zmpop<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    mpop(store, args, out, true, proto)
}

/// `LMPOP numkeys key… LEFT|RIGHT [COUNT n]`.
pub fn lmpop<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    mpop(store, args, out, false, proto)
}

/// `BZMPOP timeout numkeys key… MIN|MAX [COUNT n]`.
pub fn bzmpop<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    bmpop(store, args, out, true, proto)
}

/// `BLMPOP timeout numkeys key… LEFT|RIGHT [COUNT n]`.
pub fn blmpop<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    proto: RespVersion,
) -> Effect {
    bmpop(store, args, out, false, proto)
}

/// `ZMPOP` / `LMPOP`: the first key holding something gives up to
/// `count`; none does → a null array.
fn mpop<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    zset: bool,
    proto: RespVersion,
) -> Effect {
    if args.len() < 4 {
        wrong_args(out, if zset { "zmpop" } else { "lmpop" });
        return Effect::Unchanged;
    }
    let parsed = if zset { parse_zmpop(args, 1) } else { parse_lmpop(args, 1) };
    let p = match parsed {
        Ok(p) => p,
        Err(e) => {
            encode_error(out, e.as_wire());
            return Effect::Unchanged;
        }
    };
    for i in 2..2 + p.numkeys {
        match take(store, &args[i], zset, p) {
            Err(e) => {
                store_err(out, e);
                return Effect::Unchanged;
            }
            Ok(t) if t.is_empty() => {}
            Ok(t) => return emit(out, &args[i], &t, p, proto),
        }
    }
    out.extend_from_slice(b"*-1\r\n");
    Effect::Unchanged
}

/// `BZMPOP` / `BLMPOP timeout numkeys key… end [COUNT n]`. One key with
/// something pops as `ZMPOP` would; otherwise nothing is written and a
/// caller that can block parks the connection, as with `BLPOP`.
fn bmpop<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    zset: bool,
    proto: RespVersion,
) -> Effect {
    if args.len() < 5 {
        wrong_args(out, if zset { "bzmpop" } else { "blmpop" });
        return Effect::Unchanged;
    }
    if let Some(e) = timeout_refusal(&args[1]) {
        encode_error(out, e);
        return Effect::Unchanged;
    }
    let parsed = if zset { parse_zmpop(args, 2) } else { parse_lmpop(args, 2) };
    let p = match parsed {
        Ok(p) => p,
        Err(e) => {
            encode_error(out, e.as_wire());
            return Effect::Unchanged;
        }
    };
    if p.numkeys > 1 {
        return Effect::Unchanged;
    }
    match take(store, &args[3], zset, p) {
        Err(e) => store_err(out, e),
        Ok(t) if t.is_empty() => {}
        Ok(t) => return emit(out, &args[3], &t, p, proto),
    }
    Effect::Unchanged
}
