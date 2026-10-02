//! The keyspace events a shared-layer write publishes when they are not
//! its verb on argument 1: none for a write that changed nothing, and for
//! the pops and moves the events of what they did — the end popped, the
//! key it came from, the push before the pop of a move — as Redis names
//! them.

use kevy_resp::ArgvView;
use kevy_rt::NotifyKind;
use kevy_rt::propagation::{Notify, set_notify};
use kevy_store::Store;
use kevy_verbs::Effect;

/// Ask the runtime for this write's events, when not its default. `store`
/// is read after the write, never changed.
#[cold]
pub(crate) fn note<A: ArgvView + ?Sized>(
    cmd: &[u8],
    args: &A,
    effect: &Effect,
    reply: &[u8],
    store: &Store,
) {
    if let Some(n) = events(cmd, args, effect, reply, store) {
        set_notify(n);
    }
}

fn events<A: ArgvView + ?Sized>(
    cmd: &[u8],
    args: &A,
    effect: &Effect,
    reply: &[u8],
    store: &Store,
) -> Option<Notify> {
    // a read is never announced, and no post-write step would take its ask
    if matches!(effect, Effect::Read) {
        return None;
    }
    if reply.first() == Some(&b'-') || matches!(effect, Effect::Unchanged | Effect::Skip) {
        return Some(Notify::Suppress);
    }
    popped(cmd, args, effect)
        .or_else(|| deadlines(cmd, args, store))
        .or_else(|| written(cmd, args, reply))
}

fn one(class: NotifyKind, event: &'static str, key: &[u8]) -> Option<Notify> {
    Some(Notify::Events(vec![(class, event, key.to_vec())]))
}

/// The writes that may set a deadline: `expire` after what they wrote, or
/// `del` when the deadline had already passed and the key went.
fn deadlines<A: ArgvView + ?Sized>(cmd: &[u8], args: &A, store: &Store) -> Option<Notify> {
    let key = &args[1];
    let gone = || one(NotifyKind::Generic, "del", key);
    let set_and = |expire: bool| {
        let mut ev = vec![(NotifyKind::String, "set", key.to_vec())];
        if expire {
            ev.push((NotifyKind::Generic, "expire", key.to_vec()));
        }
        Some(Notify::Events(ev))
    };
    match cmd {
        _ if matches!(
            cmd,
            b"SET" | b"GETEX" | b"EXPIRE" | b"PEXPIRE" | b"EXPIREAT" | b"PEXPIREAT"
        ) && !store.is_live(key) =>
        {
            gone()
        }
        b"SET" => set_and(sets_deadline(args, 3)),
        b"SETEX" | b"PSETEX" => set_and(true),
        b"GETEX" if sets_deadline(args, 2) => one(NotifyKind::Generic, "expire", key),
        b"GETEX" => one(NotifyKind::Generic, "persist", key),
        b"EXPIRE" | b"PEXPIRE" | b"EXPIREAT" | b"PEXPIREAT" => {
            one(NotifyKind::Generic, "expire", key)
        }
        _ => None,
    }
}

/// Whether the `SET` / `GETEX` options from `args[from..]` name a deadline;
/// the options that take a value are stepped over whole, so a compared
/// value that reads `EX` is not taken for one.
fn sets_deadline<A: ArgvView + ?Sized>(args: &A, from: usize) -> bool {
    let mut i = from;
    while i < args.len() {
        let w = &args[i];
        let is = |name: &[u8]| w.eq_ignore_ascii_case(name);
        if is(b"EX") || is(b"PX") || is(b"EXAT") || is(b"PXAT") {
            return true;
        }
        i += if is(b"IFEQ") || is(b"IFNE") || is(b"IFDEQ") || is(b"IFDNE") { 2 } else { 1 };
    }
    false
}

/// The pops: the end they took from, on the key they took from.
fn popped<A: ArgvView + ?Sized>(cmd: &[u8], args: &A, effect: &Effect) -> Option<Notify> {
    match (cmd, effect) {
        (b"BLPOP", _) => one(NotifyKind::List, "lpop", &args[1]),
        (b"BRPOP", _) => one(NotifyKind::List, "rpop", &args[1]),
        (b"BZPOPMIN", _) => one(NotifyKind::Zset, "zpopmin", &args[1]),
        (b"BZPOPMAX", _) => one(NotifyKind::Zset, "zpopmax", &args[1]),
        // the key a multi-key pop took from is the one its record names
        (b"LMPOP" | b"BLMPOP", Effect::Record(frame)) => {
            let event = if frame[0] == b"LPOP" { "lpop" } else { "rpop" };
            one(NotifyKind::List, event, &frame[1])
        }
        (b"ZMPOP" | b"BZMPOP", Effect::Record(frame)) => {
            let at = if cmd == b"ZMPOP" { 1 } else { 2 };
            let min =
                kevy_verbs::mpop::parse_zmpop(args, at).ok()?.end == kevy_store::ListEnd::Left;
            one(NotifyKind::Zset, if min { "zpopmin" } else { "zpopmax" }, &frame[1])
        }
        _ => None,
    }
}

/// The other writes whose events are not their verb on argument 1.
fn written<A: ArgvView + ?Sized>(cmd: &[u8], args: &A, reply: &[u8]) -> Option<Notify> {
    match cmd {
        b"SETNX" | b"GETSET" => one(NotifyKind::String, "set", &args[1]),
        b"GETDEL" | b"UNLINK" if args.len() == 2 => one(NotifyKind::Generic, "del", &args[1]),
        b"INCR" | b"DECR" | b"DECRBY" => one(NotifyKind::String, "incrby", &args[1]),
        b"COPY" => one(NotifyKind::Generic, "copy_to", &args[2]),
        // an empty range removes the destination, which is a `del`
        b"ZRANGESTORE" if reply == b":0\r\n" => one(NotifyKind::Generic, "del", &args[1]),
        // a stored SORT announces its destination; an empty result removes it
        b"SORT" => {
            let dst = &args[kevy_verbs::sort::store_destination(args)?];
            if reply == b":0\r\n" {
                one(NotifyKind::Generic, "del", dst)
            } else {
                one(NotifyKind::List, "sortstore", dst)
            }
        }
        b"SMOVE" => Some(Notify::Events(vec![
            (NotifyKind::Set, "srem", args[1].to_vec()),
            (NotifyKind::Set, "sadd", args[2].to_vec()),
        ])),
        b"MSETNX" => Some(Notify::Events(
            (1..args.len())
                .step_by(2)
                .map(|i| (NotifyKind::String, "set", args[i].to_vec()))
                .collect(),
        )),
        b"BITFIELD" => one(NotifyKind::String, "setbit", &args[1]),
        // PFMERGE announces itself as an add; PFCOUNT's cache write, nothing
        b"PFMERGE" => one(NotifyKind::String, "pfadd", &args[1]),
        b"PFCOUNT" => Some(Notify::Suppress),
        b"LPUSHX" => one(NotifyKind::List, "lpush", &args[1]),
        b"RPUSHX" => one(NotifyKind::List, "rpush", &args[1]),
        // every field-TTL setter is an `hexpire`, unless its deadline had
        // passed and the fields went, which is an `hdel` (code 2)
        b"HEXPIRE" | b"HPEXPIRE" | b"HEXPIREAT" | b"HPEXPIREAT" => {
            let deleted = reply.windows(4).any(|w| w == b":2\r\n");
            one(NotifyKind::Hash, if deleted { "hdel" } else { "hexpire" }, &args[1])
        }
        b"LMOVE" | b"BLMOVE" => moved(args, &args[3], &args[4]),
        b"RPOPLPUSH" | b"BRPOPLPUSH" => moved(args, b"RIGHT", b"LEFT"),
        _ => None,
    }
}

/// A move announces the push onto the destination, then the pop off the
/// source.
fn moved<A: ArgvView + ?Sized>(args: &A, from: &[u8], to: &[u8]) -> Option<Notify> {
    let left = |end: &[u8]| end.eq_ignore_ascii_case(b"LEFT");
    let push = if left(to) { "lpush" } else { "rpush" };
    let pop = if left(from) { "lpop" } else { "rpop" };
    Some(Notify::Events(vec![
        (NotifyKind::List, push, args[2].to_vec()),
        (NotifyKind::List, pop, args[1].to_vec()),
    ]))
}
