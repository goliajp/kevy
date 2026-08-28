//! `XREAD` — the verb, its argv parse, and its reply shape.
//!
//! Split out of `mod.rs` when that file crossed the 500-line rule. The
//! seam is the verb: everything here serves XREAD and nothing else does,
//! while `emit_entries` stays behind because the group and claim paths
//! share it.

use kevy_resp::CmdError;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error};
use kevy_store::{Store, parse_explicit_id};

use crate::cmd::{store_err, wrong_args};

use super::{StreamReply, emit_entries};

/// `XREAD [COUNT n] [BLOCK ms] STREAMS key [key ...] id [id ...]`.
///
/// BLOCK semantics:
/// - `BLOCK ms` + every requested stream is empty past its requested ID:
///   leaves `out` untouched. The runtime, having resolved
///   `BlockHint::Block { kind: XReadBlock, ... }` for this command,
///   detects the no-output condition and parks the conn on the first
///   stream key. A subsequent `XADD` to that key wakes the conn and
///   re-runs this function (which now finds the new entry and emits the
///   normal multi-bulk reply); a `BLOCK 0` blocks forever.
/// - `BLOCK ms` + at least one stream has fresh entries: emits the
///   normal reply immediately (no waiter registered).
/// - No `BLOCK`: emits `*-1` on empty (non-blocking convention).
///
/// Multi-stream BLOCK on different shards is constrained by the routing
/// layer (only the first STREAMS key drives shard selection — see
/// `kevy::KevyCommands::route`). Same-shard fan-out runs inline.
pub(super) fn cmd_xread<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    // Below four arguments there is no XREAD to parse, and Redis says so in
    // its arity words rather than "syntax error" — checked against redis
    // 8.10.1, which answers `XREAD a b` with wrong number of arguments and
    // keeps "syntax error" for a call that is long enough but shaped wrong.
    if args.len() < 4 {
        return wrong_args(out, "xread");
    }
    let parsed = match parse_xread_argv(args) {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    let blocking = parsed.block_ms.is_some();
    let mut reply: Vec<StreamReply> = Vec::new();
    for (key, last_seen_arg) in parsed.streams {
        let last_seen = if last_seen_arg == b"$" {
            match store.xread_dollar_last_id(&key) {
                Ok(id) => id,
                Err(e) => return store_err(out, e),
            }
        } else {
            match parse_explicit_id(&last_seen_arg, /*end=*/ false) {
                Ok(id) => id,
                Err(_) => {
                    return encode_error(
                        out,
                        "ERR Invalid stream ID specified as stream command argument",
                    );
                }
            }
        };
        let entries = match store.xread(&key, last_seen, parsed.count) {
            Ok(es) => es,
            Err(e) => return store_err(out, e),
        };
        if !entries.is_empty() {
            reply.push((key, entries));
        }
    }
    if reply.is_empty() && blocking {
        // BLOCK + nothing fresh → leave out untouched so the dispatcher
        // registers the conn as a waiter on the first stream key (the
        // routing key — see KevyCommands::route's XREAD arm). On the
        // next XADD wake this same function re-runs and writes a reply.
        return;
    }
    emit_xread_reply(out, &reply);
}

struct XReadParsed {
    count: Option<usize>,
    /// `Some(ms)` if the client passed `BLOCK ms`; sprint D's BLOCK reactor
    /// uses this to know "no entries yet → caller should park the conn".
    /// `None` means non-blocking (`*-1` on empty).
    block_ms: Option<u64>,
    streams: Vec<(Vec<u8>, Vec<u8>)>, // (key, last-seen-arg)
}

fn parse_xread_argv<A: ArgvView + ?Sized>(args: &A) -> Result<XReadParsed, CmdError> {
    let mut count: Option<usize> = None;
    let mut block_ms: Option<u64> = None;
    let mut i = 1;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        match tok.as_slice() {
            b"COUNT" => {
                count = Some(xread_parse_kv_usize(
                    args,
                    i + 1,
                    "ERR value is not an integer or out of range",
                )?);
                i += 2;
            }
            b"BLOCK" => {
                block_ms = Some(xread_parse_kv_u64(
                    args,
                    i + 1,
                    "ERR timeout is not an integer or out of range",
                )?);
                i += 2;
            }
            b"STREAMS" => {
                let streams = xread_parse_streams(args, i + 1)?;
                return Ok(XReadParsed {
                    count,
                    block_ms,
                    streams,
                });
            }
            _ => return Err(CmdError::Wire("ERR syntax error")),
        }
    }
    Err(CmdError::Wire("ERR syntax error"))
}

fn xread_parse_kv_usize<A: ArgvView + ?Sized>(
    args: &A,
    idx: usize,
    bad: &'static str,
) -> Result<usize, CmdError> {
    let n = args.get(idx).ok_or("ERR syntax error")?;
    std::str::from_utf8(n)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or(CmdError::Wire(bad))
}

fn xread_parse_kv_u64<A: ArgvView + ?Sized>(
    args: &A,
    idx: usize,
    bad: &'static str,
) -> Result<u64, CmdError> {
    let n = args.get(idx).ok_or("ERR syntax error")?;
    std::str::from_utf8(n)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or(CmdError::Wire(bad))
}

/// `(key, last-seen-arg)` pairs as parsed from the `STREAMS …` tail.
type StreamKeyLastSeen = (Vec<u8>, Vec<u8>);

fn xread_parse_streams<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
) -> Result<Vec<StreamKeyLastSeen>, CmdError> {
    let rest = args.len() - start;
    if rest == 0 || !rest.is_multiple_of(2) {
        return Err(
            CmdError::Wire("ERR Unbalanced XREAD list of streams: for each stream key an ID or '$' must be specified."),
        );
    }
    let n = rest / 2;
    let mut streams = Vec::with_capacity(n);
    for k in 0..n {
        streams.push((args[start + k].to_vec(), args[start + n + k].to_vec()));
    }
    Ok(streams)
}


fn emit_xread_reply(out: &mut Vec<u8>, reply: &[StreamReply]) {
    if reply.is_empty() {
        // Per Redis: empty XREAD returns the null array (`*-1`).
        encode_array_len(out, -1);
        return;
    }
    encode_array_len(out, reply.len() as i64);
    for (key, entries) in reply {
        encode_array_len(out, 2);
        encode_bulk(out, key);
        emit_entries(out, entries);
    }
}
