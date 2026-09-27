//! `XCLAIM` / `XAUTOCLAIM` dispatchers — split from `group.rs` so
//! both files stay under the project's ≤500-LOC rule.

use kevy_resp::CmdError;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error};
use kevy_store::{
    EntryBatch, Store, StreamId, XClaimOpts, now_unix_ms, parse_explicit_id, parse_range_start,
};

use crate::Effect;
use crate::reply::{store_err, wrong_args};

use super::claim_record::{Before, claim_effect};
use super::emit_entries;

pub(super) fn cmd_xclaim<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 6 {
        wrong_args(out, "xclaim");
        return Effect::Write;
    }
    let Some(min_idle) = arg_min_idle(args, out) else { return Effect::Write };
    let (ids, opts, justid) = match parse_xclaim_tail(args, 5, min_idle) {
        Ok(p) => p,
        Err(msg) => {
            encode_error(out, msg.as_wire());
            return Effect::Write;
        }
    };
    let before = Before::read(store, args, &ids);
    let claimed = match store.xclaim(&args[1], &args[2], &args[3], &ids, &opts, now_unix_ms()) {
        Ok(c) => c,
        Err(e) => {
            store_err(out, e);
            return Effect::Write;
        }
    };
    emit_claim_reply(out, &claimed, justid);
    let taken: Vec<StreamId> = claimed.iter().map(|(id, _)| *id).collect();
    let dropped = before.dropped(store, &args[1], &args[2], &taken);
    claim_effect(store, args, &before, &taken, &dropped)
}

pub(super) fn cmd_xautoclaim<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 6 {
        wrong_args(out, "xautoclaim");
        return Effect::Write;
    }
    let Some(min_idle) = arg_min_idle(args, out) else { return Effect::Write };
    let Ok(start) = parse_range_start(&args[5]) else {
        encode_error(out, "ERR Invalid stream ID specified as stream command argument");
        return Effect::Write;
    };
    let (count, justid) = match parse_autoclaim_tail(args, 6) {
        Ok(p) => p,
        Err(msg) => {
            encode_error(out, msg.as_wire());
            return Effect::Write;
        }
    };
    let before = Before::read(store, args, &[]);
    let now = now_unix_ms();
    let claimed =
        store.xautoclaim(&args[1], &args[2], &args[3], min_idle, start, count, justid, now);
    let (cursor, payloads, deleted) = match claimed {
        Ok(p) => p,
        Err(e) => {
            store_err(out, e);
            return Effect::Write;
        }
    };
    emit_autoclaim_reply(out, cursor, &payloads, &deleted, justid);
    let taken: Vec<StreamId> = payloads.iter().map(|(id, _)| *id).collect();
    claim_effect(store, args, &before, &taken, &deleted)
}

/// `min-idle-time` at argv[4], or the refusal written to `out`.
fn arg_min_idle<A: ArgvView + ?Sized>(args: &A, out: &mut Vec<u8>) -> Option<u64> {
    let n = std::str::from_utf8(&args[4]).ok().and_then(|s| s.parse().ok());
    if n.is_none() {
        encode_error(out, "ERR value is not an integer or out of range");
    }
    n
}

fn parse_xclaim_tail<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    min_idle: u64,
) -> Result<(Vec<StreamId>, XClaimOpts, bool), CmdError> {
    let (ids, opt_start) = parse_xclaim_ids(args, start)?;
    let opts = parse_xclaim_opts(args, opt_start, min_idle)?;
    let justid = opts.justid;
    Ok((ids, opts, justid))
}

fn parse_xclaim_ids<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
) -> Result<(Vec<StreamId>, usize), CmdError> {
    let mut ids = Vec::new();
    let mut i = start;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        if matches!(tok.as_slice(), b"IDLE" | b"TIME" | b"RETRYCOUNT" | b"FORCE" | b"JUSTID") {
            break;
        }
        let id = parse_explicit_id(&args[i], /*end=*/ false)
            .map_err(|_| "ERR Invalid stream ID specified as stream command argument")?;
        ids.push(id);
        i += 1;
    }
    Ok((ids, i))
}

fn parse_xclaim_opts<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    min_idle: u64,
) -> Result<XClaimOpts, CmdError> {
    let mut opts = XClaimOpts {
        min_idle_ms: min_idle,
        idle_override_ms: None,
        time_override_ms: None,
        retrycount_override: None,
        force: false,
        justid: false,
    };
    let mut i = start;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        match tok.as_slice() {
            b"IDLE" => {
                opts.idle_override_ms = Some(parse_u64(args.get(i + 1))?);
                i += 2;
            }
            b"TIME" => {
                opts.time_override_ms = Some(parse_u64(args.get(i + 1))?);
                i += 2;
            }
            b"RETRYCOUNT" => {
                opts.retrycount_override = Some(parse_u64(args.get(i + 1))? as u32);
                i += 2;
            }
            b"FORCE" => {
                opts.force = true;
                i += 1;
            }
            b"JUSTID" => {
                opts.justid = true;
                i += 1;
            }
            _ => return Err(CmdError::Wire("ERR syntax error")),
        }
    }
    Ok(opts)
}

fn parse_autoclaim_tail<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
) -> Result<(usize, bool), CmdError> {
    let mut count: usize = 100;
    let mut justid = false;
    let mut i = start;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        match tok.as_slice() {
            b"COUNT" => {
                count = parse_u64(args.get(i + 1))? as usize;
                i += 2;
            }
            b"JUSTID" => {
                justid = true;
                i += 1;
            }
            _ => return Err(CmdError::Wire("ERR syntax error")),
        }
    }
    Ok((count, justid))
}

pub(super) fn parse_u64(arg: Option<&[u8]>) -> Result<u64, CmdError> {
    let v = arg.ok_or("ERR syntax error")?;
    std::str::from_utf8(v)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or(CmdError::Wire("ERR value is not an integer or out of range"))
}

fn emit_claim_reply(out: &mut Vec<u8>, claimed: &EntryBatch, justid: bool) {
    if justid {
        encode_array_len(out, claimed.len() as i64);
        for (id, _) in claimed {
            encode_bulk(out, &id.encode());
        }
    } else {
        emit_entries(out, claimed);
    }
}

fn emit_autoclaim_reply(
    out: &mut Vec<u8>,
    cursor: StreamId,
    payloads: &EntryBatch,
    deleted: &[StreamId],
    justid: bool,
) {
    encode_array_len(out, 3);
    encode_bulk(out, &cursor.encode());
    if justid {
        encode_array_len(out, payloads.len() as i64);
        for (id, _) in payloads {
            encode_bulk(out, &id.encode());
        }
    } else {
        emit_entries(out, payloads);
    }
    encode_array_len(out, deleted.len() as i64);
    for id in deleted {
        encode_bulk(out, &id.encode());
    }
}
