//! `XCLAIM` / `XAUTOCLAIM` dispatchers.
//!
//! `XCLAIM` looks its key and group up before it reads any argument;
//! `XAUTOCLAIM` reads every argument first. `XCLAIM` takes IDs until the
//! first argument that is not one, and reads options from there.

use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error};
use kevy_store::{
    ClaimMode, EntryBatch, Store, StreamId, XClaimOpts, now_unix_ms, parse_explicit_id,
};

use crate::Effect;
use crate::reply::{store_err, wrong_args};

use super::claim_record::{Before, claim_effect};
use super::emit_entries;
use super::group::no_key_or_group;
use super::opts::{BAD_ID, interval_start, strict_i64};

/// The largest `COUNT` `XAUTOCLAIM` takes: 2^43.
const AUTOCLAIM_MAX_COUNT: i64 = 1 << 43;

pub(super) fn cmd_xclaim<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 6 {
        wrong_args(out, "xclaim");
        return Effect::Write;
    }
    let (key, group) = (&args[1], &args[2]);
    match store.stream_view(key) {
        Ok(Some(s)) if s.group(group).is_some() => {}
        Ok(_) => {
            no_key_or_group(out, key, group);
            return Effect::Write;
        }
        Err(e) => {
            store_err(out, e);
            return Effect::Write;
        }
    }
    let Some(min_idle) = min_idle(&args[4], "XCLAIM", out) else { return Effect::Write };
    let (ids, opts) = match parse_xclaim_tail(args, min_idle) {
        Ok(p) => p,
        Err(e) => {
            encode_error(out, &e);
            return Effect::Write;
        }
    };
    let before = Before::read(store, args, &ids);
    let claimed = match store.xclaim(key, group, &args[3], &ids, &opts, now_unix_ms()) {
        Ok(c) => c,
        Err(e) => {
            store_err(out, e);
            return Effect::Write;
        }
    };
    emit_claim_reply(out, &claimed, opts.mode);
    let taken: Vec<StreamId> = claimed.iter().map(|(id, _)| *id).collect();
    let dropped = before.dropped(store, key, group, &taken);
    claim_effect(&before, store, args, taken, dropped)
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
    let Some(min_idle) = min_idle(&args[4], "XAUTOCLAIM", out) else { return Effect::Write };
    let start = match interval_start(&args[5]) {
        Ok(id) => id,
        Err(e) => {
            encode_error(out, e.as_wire());
            return Effect::Write;
        }
    };
    let (count, mode) = match parse_autoclaim_tail(args) {
        Ok(p) => p,
        Err(msg) => {
            encode_error(out, msg);
            return Effect::Write;
        }
    };
    let (key, group) = (&args[1], &args[2]);
    match store.stream_view(key) {
        Ok(Some(s)) if s.group(group).is_some() => {}
        Ok(_) => {
            no_key_or_group(out, key, group);
            return Effect::Write;
        }
        Err(e) => {
            store_err(out, e);
            return Effect::Write;
        }
    }
    let before = Before::read(store, args, &[]);
    let now = now_unix_ms();
    let claimed = store.xautoclaim(key, group, &args[3], min_idle, start, count, mode, now);
    let (cursor, payloads, deleted) = match claimed {
        Ok(p) => p,
        Err(e) => {
            store_err(out, e);
            return Effect::Write;
        }
    };
    emit_autoclaim_reply(out, cursor, &payloads, &deleted, mode);
    let taken: Vec<StreamId> = payloads.iter().map(|(id, _)| *id).collect();
    claim_effect(&before, store, args, taken, deleted)
}

/// `min-idle-time`, a negative one taken as 0, or the refusal written to
/// `out`.
fn min_idle(arg: &[u8], verb: &str, out: &mut Vec<u8>) -> Option<u64> {
    let Some(n) = strict_i64(arg) else {
        encode_error(out, &format!("ERR Invalid min-idle-time argument for {verb}"));
        return None;
    };
    Some(u64::try_from(n).unwrap_or(0))
}

/// The IDs from `args[5]` up to the first argument that is not one, then
/// the options.
fn parse_xclaim_tail<A: ArgvView + ?Sized>(
    args: &A,
    min_idle: u64,
) -> Result<(Vec<StreamId>, XClaimOpts), String> {
    let mut ids = Vec::new();
    let mut i = 5;
    while let Some(id) = args.get(i).and_then(|a| parse_explicit_id(a).ok()) {
        ids.push(id);
        i += 1;
    }
    let mut opts = XClaimOpts::default().with_min_idle_ms(min_idle);
    while i < args.len() {
        i += xclaim_option(args, i, &mut opts)?;
    }
    Ok((ids, opts))
}

/// One option at `args[i]` into `opts`: how many arguments it took, or
/// the refusal.
fn xclaim_option<A: ArgvView + ?Sized>(
    args: &A,
    i: usize,
    opts: &mut XClaimOpts,
) -> Result<usize, String> {
    let tok = &args[i];
    let unknown = || format!("ERR Unrecognized XCLAIM option '{}'", String::from_utf8_lossy(tok));
    let upper = tok.to_ascii_uppercase();
    if upper == b"FORCE" {
        opts.force = true;
        return Ok(1);
    }
    if upper == b"JUSTID" {
        opts.mode = ClaimMode::JustId;
        return Ok(1);
    }
    if !matches!(upper.as_slice(), b"IDLE" | b"TIME" | b"RETRYCOUNT" | b"LASTID") {
        return Err(unknown());
    }
    let v = args.get(i + 1).ok_or_else(unknown)?;
    if upper == b"LASTID" {
        opts.last_id = Some(parse_explicit_id(v).map_err(|_| BAD_ID.to_owned())?);
        return Ok(2);
    }
    let name = String::from_utf8_lossy(&upper).into_owned();
    let n =
        strict_i64(v).ok_or_else(|| format!("ERR Invalid {name} option argument for XCLAIM"))?;
    match upper.as_slice() {
        // the later of IDLE and TIME wins; a negative one is the claim's time
        b"IDLE" => {
            opts.time_override_ms = None;
            opts.idle_override_ms = Some(u64::try_from(n).unwrap_or(0));
        }
        b"TIME" => {
            opts.idle_override_ms = None;
            opts.time_override_ms = Some(u64::try_from(n).unwrap_or(u64::MAX));
        }
        // a negative count leaves the count to the claim
        _ => opts.retrycount_override = u64::try_from(n).ok(),
    }
    Ok(2)
}

/// `[COUNT n] [JUSTID]` from `args[6]`.
fn parse_autoclaim_tail<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<(usize, ClaimMode), &'static str> {
    let mut count: usize = 100;
    let mut mode = ClaimMode::Deliver;
    let mut i = 6;
    while i < args.len() {
        let tok = &args[i];
        if tok.eq_ignore_ascii_case(b"COUNT") {
            let v = args.get(i + 1).ok_or("ERR syntax error")?;
            count = strict_i64(v)
                .filter(|n| (1..=AUTOCLAIM_MAX_COUNT).contains(n))
                .map(|n| n as usize)
                .ok_or("ERR COUNT must be > 0")?;
            i += 2;
        } else if tok.eq_ignore_ascii_case(b"JUSTID") {
            mode = ClaimMode::JustId;
            i += 1;
        } else {
            return Err("ERR syntax error");
        }
    }
    Ok((count, mode))
}

fn emit_claim_reply(out: &mut Vec<u8>, claimed: &EntryBatch, mode: ClaimMode) {
    if mode == ClaimMode::JustId {
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
    mode: ClaimMode,
) {
    encode_array_len(out, 3);
    encode_bulk(out, &cursor.encode());
    if mode == ClaimMode::JustId {
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
