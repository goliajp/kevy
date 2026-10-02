//! Serve, undo and readiness for the blocking pops beyond the first set —
//! `BZPOPMAX`, `BZMPOP`, `BLMPOP`, `BLMOVE`. The first set and the
//! protocol they all follow live in [`crate::cmd_block_serve`].

use kevy_resp::{Argv, ArgvView};
use kevy_rt::{BlockKind, Store};

fn argv(parts: &[&[u8]]) -> Argv {
    let mut a = Argv::default();
    for p in parts {
        a.push(p);
    }
    a
}

/// The single-key, block-forever replay for one watched `key`; `None` for
/// a kind this file does not cover.
pub(crate) fn serve_argv<A: ArgvView + ?Sized>(
    args: &A,
    kind: BlockKind,
    key: &[u8],
) -> Option<Argv> {
    Some(match kind {
        BlockKind::Bzpopmax => argv(&[b"BZPOPMAX", key, b"0"]),
        // `VERB 0 1 key end [COUNT n]`: the tail after the keys rides along
        BlockKind::Bzmpop | BlockKind::Blmpop => {
            let numkeys: usize = std::str::from_utf8(args.get(2)?).ok()?.parse().ok()?;
            let mut a = argv(&[&args[0], b"0", b"1", key]);
            for i in 3 + numkeys..args.len() {
                a.push(&args[i]);
            }
            a
        }
        BlockKind::Blmove => {
            argv(&[b"BLMOVE", key, args.get(2)?, args.get(3)?, args.get(4)?, b"0"])
        }
        _ => return None,
    })
}

/// What replaying `serve` is about to take from `key`, as the command
/// that puts it back; read before the serve runs.
pub(crate) fn restore<A: ArgvView + ?Sized>(
    store: &mut Store,
    kind: BlockKind,
    serve: &A,
    key: &[u8],
) -> Option<Argv> {
    match kind {
        BlockKind::Bzpopmax => zadd_back(key, store.zrevrange(key, 0, 0).ok()?),
        BlockKind::Bzmpop | BlockKind::Blmpop => {
            let low = serve.get(4)?.eq_ignore_ascii_case(if kind == BlockKind::Bzmpop {
                b"MIN"
            } else {
                b"LEFT"
            });
            let n = match serve.get(6) {
                Some(c) => std::str::from_utf8(c).ok()?.parse::<i64>().ok()?,
                None => 1,
            };
            if kind == BlockKind::Bzmpop {
                let taken =
                    if low { store.zrange(key, 0, n - 1) } else { store.zrevrange(key, 0, n - 1) };
                zadd_back(key, taken.ok()?)
            } else {
                push_back(
                    key,
                    low,
                    store
                        .lrange(key, if low { 0 } else { -n }, if low { n - 1 } else { -1 })
                        .ok()?,
                )
            }
        }
        // a move parks its element in the destination, not nowhere; the
        // same answer BRPOPLPUSH gives
        _ => None,
    }
}

fn zadd_back(key: &[u8], members: Vec<(Vec<u8>, f64)>) -> Option<Argv> {
    if members.is_empty() {
        return None;
    }
    let mut a = argv(&[b"ZADD", key]);
    for (m, s) in &members {
        a.push(&crate::cmd::fmt_score(*s));
        a.push(m);
    }
    Some(a)
}

/// The elements a left pop takes come back by `LPUSH` last-first, so the
/// head ends up as it was; a right pop's by `RPUSH` in list order.
fn push_back(key: &[u8], left: bool, elements: Vec<Vec<u8>>) -> Option<Argv> {
    if elements.is_empty() {
        return None;
    }
    let mut a = argv(&[if left { b"LPUSH" } else { b"RPUSH" }, key]);
    if left {
        elements.iter().rev().for_each(|e| a.push(e));
    } else {
        elements.iter().for_each(|e| a.push(e));
    }
    Some(a)
}

/// Would replaying `serve` answer now? A key of the wrong type answers at
/// once too — with the error. `None` for a kind this file does not cover.
pub(crate) fn ready<A: ArgvView + ?Sized>(
    store: &mut Store,
    serve: &A,
    kind: BlockKind,
) -> Option<bool> {
    let (at, zset) = match kind {
        BlockKind::Bzpopmax => (1, true),
        BlockKind::Bzmpop => (3, true),
        BlockKind::Blmpop => (3, false),
        BlockKind::Blmove => (1, false),
        _ => return None,
    };
    let key = serve.get(at)?;
    let held = if zset { store.zcard(key) } else { store.llen(key) };
    Some(held.map_or(true, |n| n > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Argv {
        Argv::from(s.split(' ').map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>())
    }

    fn run(store: &mut Store, cmd: &Argv) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 32];
        let up = kevy_verbs::args::upper_verb(&cmd[0], &mut buf);
        kevy_verbs::exec(store, up, cmd, &mut out);
        out
    }

    /// Serve, then apply the undo taken before it: the key reads as it did.
    fn round_trip(kind: BlockKind, setup: &str, serve: &str, read: &str) {
        let mut s = Store::new();
        run(&mut s, &a(setup));
        let before = run(&mut s, &a(read));
        let undo = restore(&mut s, kind, &a(serve), b"k").expect("something to put back");
        assert!(!run(&mut s, &a(serve)).is_empty(), "{serve}: the serve took nothing");
        assert_ne!(run(&mut s, &a(read)), before, "{serve}: the serve changed nothing");
        run(&mut s, &undo);
        assert_eq!(run(&mut s, &a(read)), before, "{serve}: the undo did not restore");
    }

    #[test]
    fn every_undo_puts_back_what_its_serve_took() {
        let zset = "ZADD k 1 a 2 b 3 c 4 d";
        let zread = "ZRANGE k 0 -1 WITHSCORES";
        round_trip(BlockKind::Bzpopmax, zset, "BZPOPMAX k 0", zread);
        round_trip(BlockKind::Bzmpop, zset, "BZMPOP 0 1 k MIN COUNT 2", zread);
        round_trip(BlockKind::Bzmpop, zset, "BZMPOP 0 1 k MAX COUNT 3", zread);
        round_trip(BlockKind::Bzmpop, zset, "BZMPOP 0 1 k MAX", zread);
        let list = "RPUSH k a b c d";
        let lread = "LRANGE k 0 -1";
        round_trip(BlockKind::Blmpop, list, "BLMPOP 0 1 k LEFT COUNT 3", lread);
        round_trip(BlockKind::Blmpop, list, "BLMPOP 0 1 k RIGHT COUNT 2", lread);
        round_trip(BlockKind::Blmpop, list, "BLMPOP 0 1 k RIGHT", lread);
    }

    #[test]
    fn an_empty_key_has_nothing_to_undo() {
        let mut s = Store::new();
        assert!(restore(&mut s, BlockKind::Blmpop, &a("BLMPOP 0 1 k LEFT"), b"k").is_none());
        assert!(restore(&mut s, BlockKind::Bzmpop, &a("BZMPOP 0 1 k MIN"), b"k").is_none());
    }
}
