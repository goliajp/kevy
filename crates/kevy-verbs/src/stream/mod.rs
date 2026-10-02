//! The stream commands: `XADD`, `XLEN`, `XRANGE`, `XREVRANGE`, `XDEL`,
//! `XTRIM`, `XSETID`, `XREAD` and the consumer-group family (`XGROUP`,
//! `XREADGROUP`, `XACK`, `XPENDING`, `XCLAIM`, `XAUTOCLAIM`, `XINFO`),
//! over `kevy_store::StreamData`. A read with `BLOCK` that finds nothing
//! new writes no reply at all, so a caller that can park a connection
//! does so and runs the command again when the stream grows.

// The discarded value is the operation's own count — how many fields
// went, how many members landed — and the caller returns its own.
#![expect(
    clippy::let_underscore_must_use,
    reason = "the discarded value is a count, not an error report"
)]

mod add;
mod claim;
mod claim_record;
mod group;
mod info;
pub use info::xinfo;
mod opts;
mod read;
use read::cmd_xread;
mod readgroup;
pub use readgroup::xreadgroup_refusal;
mod setid;
mod xgroup;

use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer};
use kevy_store::{EntryBatch, Store, parse_explicit_id};

/// One stream's reply payload — the wire shape `XREAD` emits per
/// stream (key + entries).
pub(super) type StreamReply = (Vec<u8>, EntryBatch);

use crate::Effect;
use crate::reply::{store_err, wrong_args};

/// One stream command; `None` = the verb is not in this group.
pub(crate) fn exec<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Option<Effect> {
    match cmd {
        b"XADD" => return Some(add::cmd_xadd(store, args, out)),
        b"XLEN" => cmd_xlen(store, args, out),
        b"XRANGE" => cmd_range(store, args, out, /*rev=*/ false),
        b"XREVRANGE" => cmd_range(store, args, out, /*rev=*/ true),
        b"XDEL" => cmd_xdel(store, args, out),
        b"XTRIM" => return Some(add::cmd_xtrim(store, args, out)),
        b"XSETID" => setid::cmd_xsetid(store, args, out),
        b"XREAD" => cmd_xread(store, args, out),
        b"XGROUP" => return Some(xgroup::cmd_xgroup(store, args, out)),
        b"XREADGROUP" => return Some(readgroup::cmd_xreadgroup(store, args, out)),
        b"XACK" => group::cmd_xack(store, args, out),
        b"XPENDING" => group::cmd_xpending(store, args, out),
        b"XCLAIM" => return Some(claim::cmd_xclaim(store, args, out)),
        b"XAUTOCLAIM" => return Some(claim::cmd_xautoclaim(store, args, out)),
        b"XINFO" => info::xinfo(store, args, out, kevy_resp::RespVersion::V2),
        _ => return None,
    }
    Some(effect(cmd))
}

/// What a stream command that ran is recorded as: its argv for every
/// verb that can change a stream or a group, nothing for a read. `XADD`,
/// `XREADGROUP` and the claims decide their own record.
fn effect(cmd: &[u8]) -> Effect {
    let write = matches!(cmd, b"XDEL" | b"XSETID" | b"XACK");
    if write { Effect::Write } else { Effect::Read }
}

// ───────────── XLEN ─────────────

fn cmd_xlen<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return wrong_args(out, "xlen");
    }
    match store.xlen(&args[1]) {
        Ok(n) => encode_integer(out, n as i64),
        Err(e) => store_err(out, e),
    }
}

// ───────────── XRANGE / XREVRANGE ─────────────

/// `XRANGE key start end [COUNT n]`, `XREVRANGE key end start [COUNT
/// n]`: every argument is read before the key is looked at. A `COUNT` of
/// zero or less answers the null array on a stream that exists.
fn cmd_range<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>, rev: bool) {
    if args.len() < 4 {
        return wrong_args(out, if rev { "xrevrange" } else { "xrange" });
    }
    let (s_arg, e_arg) = if rev { (&args[3], &args[2]) } else { (&args[2], &args[3]) };
    let bounds = opts::interval_start(s_arg).and_then(|s| Ok((s, opts::interval_end(e_arg)?)));
    let (start, end) = match bounds {
        Ok(b) => b,
        Err(e) => return encode_error(out, e.as_wire()),
    };
    let mut count = None;
    let mut i = 4;
    while i < args.len() {
        if !args[i].eq_ignore_ascii_case(b"COUNT") || i + 1 == args.len() {
            return encode_error(out, "ERR syntax error");
        }
        match opts::strict_i64(&args[i + 1]) {
            Some(n) => count = Some(n),
            None => return encode_error(out, crate::reply::ERR_NOT_INT),
        }
        i += 2;
    }
    if let Some(n) = count.filter(|n| *n <= 0) {
        let _ = n;
        return match store.stream_view(&args[1]) {
            Ok(Some(_)) => encode_array_len(out, -1),
            Ok(None) => encode_array_len(out, 0),
            Err(e) => store_err(out, e),
        };
    }
    let count = count.map(|n| n as usize);
    let entries = if rev {
        store.xrevrange(&args[1], start, end, count)
    } else {
        store.xrange(&args[1], start, end, count)
    };
    match entries {
        Ok(es) => emit_entries(out, &es),
        Err(e) => store_err(out, e),
    }
}

// ───────────── XDEL / XTRIM ─────────────

fn cmd_xdel<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "xdel");
    }
    let mut ids = Vec::with_capacity(args.len() - 2);
    for i in 2..args.len() {
        match parse_explicit_id(&args[i]) {
            Ok(id) => ids.push(id),
            Err(_) => {
                return encode_error(
                    out,
                    "ERR Invalid stream ID specified as stream command argument",
                );
            }
        }
    }
    match store.xdel(&args[1], &ids) {
        Ok(n) => encode_integer(out, n as i64),
        Err(e) => store_err(out, e),
    }
}

// ───────────── XREAD (non-blocking) ─────────────

// ───────────── reply emitters ─────────────

pub(super) fn emit_entries(out: &mut Vec<u8>, entries: &EntryBatch) {
    encode_array_len(out, entries.len() as i64);
    for (id, fv) in entries {
        encode_array_len(out, 2);
        encode_bulk(out, &id.encode());
        encode_array_len(out, (fv.len() * 2) as i64);
        for (f, v) in fv {
            encode_bulk(out, f);
            encode_bulk(out, v);
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_info;
#[cfg(test)]
mod tests_trim_record;
