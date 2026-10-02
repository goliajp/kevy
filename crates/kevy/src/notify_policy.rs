//! The keyspace events a shared-layer write publishes when they are not
//! its verb on argument 1: none for a write that changed nothing, and for
//! the pops and moves the events of what they did — the end popped, the
//! key it came from, the push before the pop of a move — as Redis names
//! them.

use kevy_resp::ArgvView;
use kevy_rt::NotifyKind;
use kevy_rt::propagation::{Notify, set_notify};
use kevy_verbs::Effect;

/// Ask the runtime for this write's events, when not its default.
#[cold]
pub(crate) fn note<A: ArgvView + ?Sized>(cmd: &[u8], args: &A, effect: &Effect, reply: &[u8]) {
    if let Some(n) = events(cmd, args, effect, reply) {
        set_notify(n);
    }
}

fn events<A: ArgvView + ?Sized>(
    cmd: &[u8],
    args: &A,
    effect: &Effect,
    reply: &[u8],
) -> Option<Notify> {
    // a read is never announced, and no post-write step would take its ask
    if matches!(effect, Effect::Read) {
        return None;
    }
    if reply.first() == Some(&b'-') || matches!(effect, Effect::Unchanged | Effect::Skip) {
        return Some(Notify::Suppress);
    }
    let one = |class, event, key: &[u8]| Some(Notify::Events(vec![(class, event, key.to_vec())]));
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
        (b"LMOVE" | b"BLMOVE", _) => moved(args, &args[3], &args[4]),
        (b"RPOPLPUSH" | b"BRPOPLPUSH", _) => moved(args, b"RIGHT", b"LEFT"),
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
