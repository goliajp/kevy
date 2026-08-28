//! `XADD` / `XLEN` / `XRANGE` / `XREVRANGE` / `XDEL` / `XTRIM` /
//! `XREAD` + the consumer-group family (`XGROUP` / `XREADGROUP` /
//! `XACK` / `XPENDING` / `XCLAIM` / `XAUTOCLAIM`) — v2-7 sprints A
//! (basics) and B (groups). XINFO + blocking reads land in sprints C
//! and D respectively. Backed by `kevy_store::StreamData` — no new
//! store value variant.
//!
//! Sub-modules:
//! - `mod.rs` (this file) — dispatch entry + sprint A commands.
//! - `group.rs` — sprint B (consumer-group commands).

mod claim;
mod group;
mod info;
mod read;
use read::cmd_xread;
mod setid;

use kevy_resp::CmdError;
use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
};
use kevy_store::{
    EntryBatch, Store, StreamId, XAddIdSpec, now_unix_ms, parse_explicit_id, parse_range_end,
    parse_range_start, parse_xadd_id,
};

/// One stream's reply payload — the wire shape `XREAD` emits per
/// stream (key + entries).
pub(super) type StreamReply = (Vec<u8>, EntryBatch);

use crate::cmd::{store_err, wrong_args};

/// Dispatch table for the basic XADD/range/read verbs. Returns `true`
/// if `cmd` matched (and a reply was written).
pub(crate) fn dispatch_stream<A: ArgvView + ?Sized>(
    cmd: &[u8],
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> bool {
    match cmd {
        b"XADD" => cmd_xadd(store, args, out),
        b"XLEN" => cmd_xlen(store, args, out),
        b"XRANGE" => cmd_range(store, args, out, /*rev=*/ false),
        b"XREVRANGE" => cmd_range(store, args, out, /*rev=*/ true),
        b"XDEL" => cmd_xdel(store, args, out),
        b"XTRIM" => cmd_xtrim(store, args, out),
        b"XSETID" => setid::cmd_xsetid(store, args, out),
        b"XREAD" => cmd_xread(store, args, out),
        b"XGROUP" => group::cmd_xgroup(store, args, out),
        b"XREADGROUP" => group::cmd_xreadgroup(store, args, out),
        b"XACK" => group::cmd_xack(store, args, out),
        b"XPENDING" => group::cmd_xpending(store, args, out),
        b"XCLAIM" => claim::cmd_xclaim(store, args, out),
        b"XAUTOCLAIM" => claim::cmd_xautoclaim(store, args, out),
        b"XINFO" => info::cmd_xinfo(store, args, out),
        _ => return false,
    }
    true
}

// ───────────── XADD ─────────────

/// `XADD key [NOMKSTREAM] [MAXLEN [=|~] N | MINID [=|~] id [LIMIT N]]
/// <id|*> field value [field value ...]`
fn cmd_xadd<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 5 {
        return wrong_args(out, "xadd");
    }
    let parsed = match parse_xadd_argv(args) {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    let id = match store.xadd(
        &args[1],
        parsed.id,
        parsed.fields,
        parsed.nomkstream,
        now_unix_ms(),
    ) {
        Ok(Some(id)) => id,
        Ok(None) => return encode_null_bulk(out), // NOMKSTREAM + missing key
        Err(kevy_store::StoreError::OutOfRange) => {
            return encode_error(
                out,
                "ERR The ID specified in XADD is equal or smaller than the target stream top item",
            );
        }
        Err(e) => return store_err(out, e),
    };
    if let Some(trim) = parsed.trim {
        apply_trim(store, &args[1], trim);
    }
    encode_bulk(out, &id.encode());
}

struct XAddParsed {
    nomkstream: bool,
    trim: Option<TrimSpec>,
    id: XAddIdSpec,
    fields: Vec<(Vec<u8>, Vec<u8>)>,
}

enum TrimSpec {
    MaxLen(u64),
    MinId(StreamId),
}

fn parse_xadd_argv<A: ArgvView + ?Sized>(args: &A) -> Result<XAddParsed, CmdError> {
    let mut i = 2;
    let mut nomkstream = false;
    let mut trim: Option<TrimSpec> = None;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        match tok.as_slice() {
            b"NOMKSTREAM" => {
                nomkstream = true;
                i += 1;
            }
            b"MAXLEN" => {
                let (spec, used) = parse_trim_arg(args, i + 1, /*maxlen=*/ true)?;
                trim = Some(spec);
                i += 1 + used;
            }
            b"MINID" => {
                let (spec, used) = parse_trim_arg(args, i + 1, /*maxlen=*/ false)?;
                trim = Some(spec);
                i += 1 + used;
            }
            _ => break,
        }
    }
    if i + 2 >= args.len() {
        return Err(CmdError::Wire("ERR wrong number of arguments for 'xadd' command"));
    }
    let id = parse_xadd_id(&args[i]).map_err(|_| {
        "ERR Invalid stream ID specified as stream command argument"
    })?;
    i += 1;
    let rest = args.len() - i;
    if !rest.is_multiple_of(2) || rest == 0 {
        return Err(CmdError::Wire("ERR wrong number of arguments for 'xadd' command"));
    }
    let mut fields = Vec::with_capacity(rest / 2);
    while i < args.len() {
        fields.push((args[i].to_vec(), args[i + 1].to_vec()));
        i += 2;
    }
    Ok(XAddParsed { nomkstream, trim, id, fields })
}

/// Skip the optional `=` / `~` modifier and parse the trim threshold.
/// `~` is "approximate" in Redis (radix-tree-aligned); we accept it for
/// wire compatibility but always trim exactly. Returns the number of
/// args consumed (1 or 2).
fn parse_trim_arg<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
    maxlen: bool,
) -> Result<(TrimSpec, usize), CmdError> {
    let mut used = 0usize;
    let mut idx = start;
    if let Some(t) = args.get(idx)
        && (t == b"=" || t == b"~")
    {
        idx += 1;
        used += 1;
    }
    let val = args.get(idx).ok_or("ERR syntax error")?;
    used += 1;
    if maxlen {
        let n: u64 = std::str::from_utf8(val)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or("ERR value is not an integer or out of range")?;
        Ok((TrimSpec::MaxLen(n), used))
    } else {
        let id = parse_explicit_id(val, /*end=*/ false)
            .map_err(|_| "ERR Invalid stream ID specified as stream command argument")?;
        Ok((TrimSpec::MinId(id), used))
    }
}

fn apply_trim(store: &mut Store, key: &[u8], trim: TrimSpec) {
    let _ = match trim {
        TrimSpec::MaxLen(n) => store.xtrim_maxlen(key, n),
        TrimSpec::MinId(id) => store.xtrim_minid(key, id),
    };
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

fn cmd_range<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
    rev: bool,
) {
    if !(4..=6).contains(&args.len()) {
        return wrong_args(out, if rev { "xrevrange" } else { "xrange" });
    }
    // XREVRANGE swaps start/end at the argv layer (high → low).
    let (s_arg, e_arg) = if rev {
        (&args[3], &args[2])
    } else {
        (&args[2], &args[3])
    };
    let Ok(start) = parse_range_start(s_arg) else {
        return encode_error(
            out,
            "ERR Invalid stream ID specified as stream command argument",
        );
    };
    let Ok(end) = parse_range_end(e_arg) else {
        return encode_error(
            out,
            "ERR Invalid stream ID specified as stream command argument",
        );
    };
    let count = match parse_optional_count(args, 4) {
        Ok(c) => c,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    let entries = match (rev, store.xrange(&args[1], start, end, count)) {
        (false, Ok(es)) => es,
        (true, _) => match store.xrevrange(&args[1], start, end, count) {
            Ok(es) => es,
            Err(e) => return store_err(out, e),
        },
        (false, Err(e)) => return store_err(out, e),
    };
    emit_entries(out, &entries);
}

/// Decode an optional `COUNT n` tail at argv index `start` (the `COUNT`
/// literal is at index `start`, the integer at `start + 1`). Returns
/// `Ok(None)` when the tail is absent.
fn parse_optional_count<A: ArgvView + ?Sized>(
    args: &A,
    start: usize,
) -> Result<Option<usize>, CmdError> {
    if start >= args.len() {
        return Ok(None);
    }
    if !args[start].eq_ignore_ascii_case(b"COUNT") {
        return Err(CmdError::Wire("ERR syntax error"));
    }
    let n = args.get(start + 1).ok_or("ERR syntax error")?;
    let n: usize = std::str::from_utf8(n)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("ERR value is not an integer or out of range")?;
    Ok(Some(n))
}

// ───────────── XDEL / XTRIM ─────────────

fn cmd_xdel<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "xdel");
    }
    let mut ids = Vec::with_capacity(args.len() - 2);
    for i in 2..args.len() {
        match parse_explicit_id(&args[i], /*end=*/ false) {
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

fn cmd_xtrim<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 4 {
        return wrong_args(out, "xtrim");
    }
    let mode = args[2].to_ascii_uppercase();
    let spec = match mode.as_slice() {
        b"MAXLEN" => match parse_trim_arg(args, 3, /*maxlen=*/ true) {
            Ok((s, _)) => s,
            Err(msg) => return encode_error(out, msg.as_wire()),
        },
        b"MINID" => match parse_trim_arg(args, 3, /*maxlen=*/ false) {
            Ok((s, _)) => s,
            Err(msg) => return encode_error(out, msg.as_wire()),
        },
        _ => return encode_error(out, "ERR syntax error"),
    };
    let n = match spec {
        TrimSpec::MaxLen(n) => store.xtrim_maxlen(&args[1], n),
        TrimSpec::MinId(id) => store.xtrim_minid(&args[1], id),
    };
    match n {
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

