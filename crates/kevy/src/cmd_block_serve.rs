//! Cross-shard BLOCK helpers that bridge `kevy_rt`'s arbiter to kevy's
//! command syntax: build the single-key **replay** command for one watched
//! key, and the non-destructive **readiness** peek for it. Lifted out of
//! `cmd_block.rs` to keep both files under the 500-LOC house rule.
//!
//! The runtime drives these via the [`kevy_rt::Commands`] hooks
//! `block_serve_argv` (origin, park time) and `block_ready` (target, arm
//! time) — see `kevy_rt::block_xshard` for the protocol.

use kevy_resp::{Argv, ArgvView};
use kevy_rt::{BlockKind, Store};

/// Build the single-key command the arbiter replays to satisfy one watched
/// `key`. `args` is the original (possibly multi-key) command; `$` is left
/// literal here and frozen later on the key's owning shard. The `BLOCK`
/// clause is preserved for the stream forms so the one-shot replay leaves
/// no output when the key is empty (the arbiter reads that as "raced").
pub(crate) fn block_serve_argv<A: ArgvView + ?Sized>(
    args: &A,
    kind: BlockKind,
    key: &[u8],
) -> Argv {
    match kind {
        BlockKind::Blpop => pop_serve(b"BLPOP", key),
        BlockKind::Brpop => pop_serve(b"BRPOP", key),
        BlockKind::Bzpopmin => pop_serve(b"BZPOPMIN", key),
        BlockKind::Brpoplpush => brpoplpush_serve(args, key),
        BlockKind::XReadBlock => xread_serve(args, key).unwrap_or_else(|| args.to_argv()),
        BlockKind::XReadGroupBlock => xreadgroup_serve(args, key).unwrap_or_else(|| args.to_argv()),
    }
}

/// `BRPOPLPUSH src dst 0` — single-key form for the replay. `key`
/// (passed by the arbiter) is the source that woke; the destination
/// is at `args[2]` of the original `BRPOPLPUSH source destination
/// timeout` argv.
fn brpoplpush_serve<A: ArgvView + ?Sized>(args: &A, key: &[u8]) -> Argv {
    let mut a = Argv::default();
    a.push(b"BRPOPLPUSH");
    a.push(key);
    if let Some(dst) = args.get(2) {
        a.push(dst);
    } else {
        // Malformed original — fall back to a no-op key so dispatch
        // emits the args error rather than panicking.
        a.push(b"");
    }
    a.push(b"0");
    a
}

/// `BLPOP key 0` / `BRPOP key 0` — a single-key, block-forever replay; the
/// arbiter has already decided when to run it, so the embedded timeout is
/// inert (the dispatch is one-shot: pop on hit, no output on miss).
fn pop_serve(verb: &[u8], key: &[u8]) -> Argv {
    let mut a = Argv::default();
    a.push(verb);
    a.push(key);
    a.push(b"0");
    a
}

/// The command that undoes what replaying `serve_argv` is about to
/// consume, read from `store` **before** the serve runs.
///
/// A cross-shard serve pops on the target and ships the reply to the
/// origin; if the origin's client disconnected in that window, the
/// element would be lost. The target captures this undo first and holds
/// it until the origin confirms delivery. See
/// `kevy_rt::block_xshard` for the protocol.
///
/// Built from typed reads rather than by parsing the reply back apart.
/// A reply's shape depends on the kind *and* on whether the waiter
/// negotiated RESP2 or RESP3, so a parser would have to be right about
/// every combination forever; `LINDEX` means the same thing in both.
/// The peek runs on the owning shard immediately before the pop, with
/// nothing interleaved, so what it reads is what the pop takes.
pub(crate) fn block_restore_argv(store: &mut Store, kind: BlockKind, key: &[u8]) -> Option<Argv> {
    match kind {
        // BLPOP takes the head, so putting it back is an LPUSH.
        BlockKind::Blpop => push_restore(store, b"LPUSH", key, 0),
        // BRPOP takes the tail — RPUSH, and index -1.
        BlockKind::Brpop => push_restore(store, b"RPUSH", key, -1),
        BlockKind::Bzpopmin => {
            let (member, score) = store.zrange(key, 0, 0).ok()?.into_iter().next()?;
            let mut a = Argv::default();
            a.push(b"ZADD");
            a.push(key);
            a.push(&crate::cmd::fmt_score(score));
            a.push(&member);
            Some(a)
        }
        // Cross-shard BRPOPLPUSH does not come through this path at all
        // — it is served by the list-move orchestrator
        // (`serve_via_list_move`), which owns its own recovery.
        BlockKind::Brpoplpush => None,
        // XREAD is non-destructive and XREADGROUP moves entries into a
        // PEL rather than consuming them. Nothing to put back.
        BlockKind::XReadBlock | BlockKind::XReadGroupBlock => None,
    }
}

/// `LPUSH`/`RPUSH key <elem at idx>` — None when the list is empty,
/// which means the serve is about to race and produce no reply anyway.
fn push_restore(store: &mut Store, verb: &[u8], key: &[u8], idx: i64) -> Option<Argv> {
    let elem = store.lindex(key, idx).ok()??;
    let mut a = Argv::default();
    a.push(verb);
    a.push(key);
    a.push(&elem);
    Some(a)
}

/// Options scanned out of an `XREAD` / `XREADGROUP` option preamble.
#[derive(Default)]
struct StreamOpts {
    count: Option<Vec<u8>>,
    block_ms: Option<Vec<u8>>,
    noack: bool,
    /// Index of the `STREAMS` token.
    streams_at: usize,
}

/// Scan the option preamble (`COUNT` / `BLOCK` / `NOACK`) starting at `from`
/// up to `STREAMS`. `None` on an unknown token or a missing operand.
fn scan_stream_opts<A: ArgvView + ?Sized>(args: &A, from: usize) -> Option<StreamOpts> {
    let mut o = StreamOpts::default();
    let mut i = from;
    loop {
        match args.get(i)?.to_ascii_uppercase().as_slice() {
            b"COUNT" => {
                o.count = Some(args.get(i + 1)?.to_vec());
                i += 2;
            }
            b"BLOCK" => {
                o.block_ms = Some(args.get(i + 1)?.to_vec());
                i += 2;
            }
            b"NOACK" => {
                o.noack = true;
                i += 1;
            }
            b"STREAMS" => {
                o.streams_at = i;
                return Some(o);
            }
            _ => return None,
        }
    }
}

/// Append `[COUNT n] [NOACK] [BLOCK ms] STREAMS key id` to a serve argv.
fn push_stream_tail(a: &mut Argv, o: &StreamOpts, key: &[u8], id: &[u8]) {
    if let Some(c) = &o.count {
        a.push(b"COUNT");
        a.push(c);
    }
    if o.noack {
        a.push(b"NOACK");
    }
    if let Some(b) = &o.block_ms {
        a.push(b"BLOCK");
        a.push(b);
    }
    a.push(b"STREAMS");
    a.push(key);
    a.push(id);
}

/// Reconstruct `XREAD [COUNT n] BLOCK ms STREAMS key id` for one stream of
/// a (possibly multi-stream) `XREAD`. `None` on malformed input.
fn xread_serve<A: ArgvView + ?Sized>(args: &A, key: &[u8]) -> Option<Argv> {
    let o = scan_stream_opts(args, 1)?;
    let id = id_for_key(args, o.streams_at + 1, key)?;
    let mut a = Argv::default();
    a.push(b"XREAD");
    push_stream_tail(&mut a, &o, key, &id);
    Some(a)
}

/// Reconstruct `XREADGROUP GROUP g c [COUNT n] [NOACK] BLOCK ms STREAMS
/// key id` for one stream of a multi-stream `XREADGROUP`. `None` on
/// malformed input.
fn xreadgroup_serve<A: ArgvView + ?Sized>(args: &A, key: &[u8]) -> Option<Argv> {
    if args.len() < 4 || !args[1].eq_ignore_ascii_case(b"GROUP") {
        return None;
    }
    let group = args[2].to_vec();
    let consumer = args[3].to_vec();
    let o = scan_stream_opts(args, 4)?;
    let id = id_for_key(args, o.streams_at + 1, key)?;
    let mut a = Argv::default();
    a.push(b"XREADGROUP");
    a.push(b"GROUP");
    a.push(&group);
    a.push(&consumer);
    push_stream_tail(&mut a, &o, key, &id);
    Some(a)
}

/// The ID paired with `key` in a `STREAMS k1 … kn id1 … idn` tail starting
/// at `keys_start`. `None` if unbalanced or `key` is absent.
fn id_for_key<A: ArgvView + ?Sized>(args: &A, keys_start: usize, key: &[u8]) -> Option<Vec<u8>> {
    let rest = args.len().checked_sub(keys_start)?;
    if rest == 0 || !rest.is_multiple_of(2) {
        return None;
    }
    let n = rest / 2;
    let pos = (keys_start..keys_start + n).position(|i| &args[i] == key)?;
    args.get(keys_start + n + pos).map(<[u8]>::to_vec)
}

/// Non-destructive readiness peek for a frozen single-key `serve_argv`:
/// would replaying it yield a reply right now?
/// - `BLPOP`/`BRPOP` → the list at `serve_argv[1]` is non-empty.
/// - `XREAD` → re-run the (read-only) replay and check it produced output.
/// - `XREADGROUP` → the group has entries past its last-delivered id.
pub(crate) fn block_ready<A: ArgvView + ?Sized>(
    ctx: &crate::state::Ctx<'_>,
    store: &mut Store,
    serve_argv: &A,
    kind: BlockKind,
) -> bool {
    match kind {
        BlockKind::Blpop | BlockKind::Brpop | BlockKind::Brpoplpush => {
            serve_argv.get(1).is_some_and(|k| store.llen(k).is_ok_and(|n| n > 0))
        }
        BlockKind::Bzpopmin => {
            serve_argv.get(1).is_some_and(|k| store.zcard(k).is_ok_and(|n| n > 0))
        }
        BlockKind::XReadBlock => {
            // XREAD is read-only, so dispatching the replay is itself a safe
            // peek. What it is NOT is empty when there is nothing: measured,
            // `XREAD COUNT 1 STREAMS st 0` against a stream with no new
            // entries writes `*-1\r\n`, the RESP nil array, five bytes. So
            // `!tmp.is_empty()` was true for every armed XREAD — the peek
            // always said ready.
            //
            // Nothing user-visible came of it: an end-to-end `XREAD BLOCK
            // 1000` still blocks its full second, because a waiter woken with
            // nothing to serve re-arms. The cost was a cross-shard signal and
            // a re-arm per armed waiter, every time, for a question that was
            // never actually being asked.
            let mut tmp = Vec::new();
            crate::dispatch::dispatch_into(ctx, store, serve_argv, &mut tmp);
            !tmp.is_empty() && tmp != b"*-1\r\n" && tmp != b"*0\r\n"
        }
        BlockKind::XReadGroupBlock => xreadgroup_ready(store, serve_argv),
    }
}

/// `XREADGROUP … >` readiness: locate the group name and STREAMS key in
/// the frozen replay, then ask the store (non-destructively) whether the
/// group has new entries.
fn xreadgroup_ready<A: ArgvView + ?Sized>(store: &mut Store, serve_argv: &A) -> bool {
    if serve_argv.len() < 3 || !serve_argv[1].eq_ignore_ascii_case(b"GROUP") {
        return false;
    }
    let group = serve_argv[2].to_vec();
    let mut i = 4usize;
    while i < serve_argv.len() {
        if serve_argv[i].eq_ignore_ascii_case(b"STREAMS") {
            let Some(key) = serve_argv.get(i + 1) else {
                return false;
            };
            return store.xreadgroup_has_new(key, &group).unwrap_or(false);
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod restore_tests {
    use super::*;

    fn argv_strings(a: &Argv) -> Vec<Vec<u8>> {
        (0..a.len()).filter_map(|i| a.get(i).map(<[u8]>::to_vec)).collect()
    }

    #[test]
    fn blpop_restores_the_head_with_lpush() {
        let mut s = Store::default();
        s.rpush(b"q", &[b"first" as &[u8], b"second"]).unwrap();
        let undo = block_restore_argv(&mut s, BlockKind::Blpop, b"q").unwrap();
        assert_eq!(argv_strings(&undo), vec![b"LPUSH".to_vec(), b"q".to_vec(), b"first".to_vec()]);
    }

    #[test]
    fn brpop_restores_the_tail_with_rpush() {
        let mut s = Store::default();
        s.rpush(b"q", &[b"first" as &[u8], b"second"]).unwrap();
        let undo = block_restore_argv(&mut s, BlockKind::Brpop, b"q").unwrap();
        assert_eq!(argv_strings(&undo), vec![b"RPUSH".to_vec(), b"q".to_vec(), b"second".to_vec()]);
    }

    /// The undo has to name the member the pop will actually take, not
    /// just any member — BZPOPMIN takes the lowest score.
    #[test]
    fn bzpopmin_restores_the_minimum_with_its_score() {
        let mut s = Store::default();
        s.zadd(b"z", &[(2.0, b"high" as &[u8]), (1.0, b"low")]).unwrap();
        let undo = block_restore_argv(&mut s, BlockKind::Bzpopmin, b"z").unwrap();
        assert_eq!(
            argv_strings(&undo),
            vec![b"ZADD".to_vec(), b"z".to_vec(), b"1".to_vec(), b"low".to_vec()]
        );
    }

    /// Peeking must not consume. If it did, the undo would be captured
    /// by removing the very element it exists to protect.
    #[test]
    fn capturing_the_undo_does_not_mutate() {
        let mut s = Store::default();
        s.rpush(b"q", &[b"a" as &[u8], b"b"]).unwrap();
        s.zadd(b"z", &[(1.0, b"m" as &[u8])]).unwrap();
        block_restore_argv(&mut s, BlockKind::Blpop, b"q").unwrap();
        block_restore_argv(&mut s, BlockKind::Brpop, b"q").unwrap();
        block_restore_argv(&mut s, BlockKind::Bzpopmin, b"z").unwrap();
        assert_eq!(s.llen(b"q").unwrap(), 2);
        assert_eq!(s.zcard(b"z").unwrap(), 1);
    }

    #[test]
    fn an_empty_key_has_nothing_to_restore() {
        let mut s = Store::default();
        assert!(block_restore_argv(&mut s, BlockKind::Blpop, b"missing").is_none());
        assert!(block_restore_argv(&mut s, BlockKind::Bzpopmin, b"missing").is_none());
    }

    /// XREAD is non-destructive and XREADGROUP moves entries to a PEL
    /// rather than consuming them; BRPOPLPUSH is served by the list-move
    /// orchestrator and recovers itself. None of them have an undo, and
    /// inventing one would put an element back that was never taken.
    #[test]
    fn kinds_that_consume_nothing_have_no_undo() {
        let mut s = Store::default();
        s.rpush(b"q", &[b"a" as &[u8]]).unwrap();
        for kind in [BlockKind::XReadBlock, BlockKind::XReadGroupBlock, BlockKind::Brpoplpush] {
            assert!(block_restore_argv(&mut s, kind, b"q").is_none(), "{kind:?}");
        }
    }
}

#[cfg(test)]
mod ready_tests {
    use super::*;

    /// Every `BlockKind` arm of [`block_ready`], asked both ways.
    ///
    /// The arms are reachable in the wider suite only when a blocked client
    /// of that particular kind happens to be served inside a test's window,
    /// so how many of them execute is a matter of timing — this symbol grew
    /// from 3 dead regions to 11 on a CI run that touched nothing near it.
    /// Which kinds exist is not a matter of timing, and asking each one
    /// directly costs nothing.
    fn argv(parts: &[&[u8]]) -> Argv {
        Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
    }

    #[test]
    fn every_block_kind_answers_both_ways() {
        let kevy = crate::KevyCommands::default();
        let ctx = kevy.ctx();
        let mut s = Store::default();

        // List kinds: ready exactly when the key has elements.
        for kind in [BlockKind::Blpop, BlockKind::Brpop, BlockKind::Brpoplpush] {
            assert!(!block_ready(&ctx, &mut s, &argv(&[b"BLPOP", b"missing"]), kind));
        }
        s.rpush(b"q", &[b"one" as &[u8]]).unwrap();
        for kind in [BlockKind::Blpop, BlockKind::Brpop, BlockKind::Brpoplpush] {
            assert!(block_ready(&ctx, &mut s, &argv(&[b"BLPOP", b"q"]), kind));
        }

        // Sorted-set kind: ready exactly when the zset has members.
        assert!(!block_ready(&ctx, &mut s, &argv(&[b"BZPOPMIN", b"z"]), BlockKind::Bzpopmin));
        s.zadd(b"z", &[(1.0, b"m" as &[u8])]).unwrap();
        assert!(block_ready(&ctx, &mut s, &argv(&[b"BZPOPMIN", b"z"]), BlockKind::Bzpopmin));

        // XREAD peeks by dispatching the read-only replay: empty output is
        // "nothing yet", which is the only thing that makes the peek safe.
        let xread = argv(&[b"XREAD", b"COUNT", b"1", b"STREAMS", b"st", b"0"]);
        assert!(!block_ready(&ctx, &mut s, &xread, BlockKind::XReadBlock));
        kevy.dispatch(&mut s, &argv(&[b"XADD", b"st", b"1-1", b"f", b"v"]));
        assert!(block_ready(&ctx, &mut s, &xread, BlockKind::XReadBlock));

        // XREADGROUP: a call too short to name a group is not ready, and
        // cannot be — that guard is the first thing the arm does.
        assert!(!block_ready(&ctx, &mut s, &argv(&[b"XREADGROUP"]), BlockKind::XReadGroupBlock));
        let grouped =
            argv(&[b"XREADGROUP", b"GROUP", b"g", b"c", b"COUNT", b"1", b"STREAMS", b"st", b">"]);
        assert!(!block_ready(&ctx, &mut s, &grouped, BlockKind::XReadGroupBlock));
        kevy.dispatch(&mut s, &argv(&[b"XGROUP", b"CREATE", b"st", b"g", b"0"]));
        assert!(block_ready(&ctx, &mut s, &grouped, BlockKind::XReadGroupBlock));
    }
}
