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
        BlockKind::XReadBlock => xread_serve(args, key).unwrap_or_else(|| args.to_argv()),
        BlockKind::XReadGroupBlock => {
            xreadgroup_serve(args, key).unwrap_or_else(|| args.to_argv())
        }
    }
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
    store: &mut Store,
    serve_argv: &A,
    kind: BlockKind,
) -> bool {
    match kind {
        BlockKind::Blpop | BlockKind::Brpop => serve_argv
            .get(1)
            .is_some_and(|k| store.llen(k).is_ok_and(|n| n > 0)),
        BlockKind::XReadBlock => {
            // XREAD is read-only, so dispatching the replay is itself a
            // safe peek: non-empty output ⇒ data is available.
            let mut tmp = Vec::new();
            crate::dispatch::dispatch_into(store, serve_argv, &mut tmp);
            !tmp.is_empty()
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
