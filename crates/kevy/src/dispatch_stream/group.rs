//! Consumer-group dispatch: `XGROUP` / `XREADGROUP` / `XACK` /
//! `XPENDING` / `XCLAIM` / `XAUTOCLAIM` — sprint B of v2-7. Argv-soup
//! parsers translate the legacy Redis shapes into the structured
//! API on `Store` (see `kevy_store::stream::store`); reply emitters
//! match the exact array shapes Redis returns.

use kevy_resp::{
    ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
    encode_simple_string,
};
use kevy_store::{
    GroupCreateMode, ReadGroupId, Store, StreamId, now_unix_ms, parse_explicit_id,
    parse_range_end, parse_range_start,
};

use crate::cmd::{store_err, wrong_args};

use super::emit_entries;

// ───────────── XGROUP ─────────────

/// `XGROUP CREATE | DESTROY | SETID | CREATECONSUMER | DELCONSUMER`
pub(super) fn cmd_xgroup<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() < 2 {
        return wrong_args(out, "xgroup");
    }
    let sub = args[1].to_ascii_uppercase();
    match sub.as_slice() {
        b"CREATE" => xgroup_create(store, args, out),
        b"DESTROY" => xgroup_destroy(store, args, out),
        b"SETID" => xgroup_setid(store, args, out),
        b"CREATECONSUMER" => xgroup_create_consumer(store, args, out),
        b"DELCONSUMER" => xgroup_del_consumer(store, args, out),
        other => encode_error(
            out,
            &format!(
                "ERR Unknown XGROUP subcommand or wrong number of arguments for '{}'",
                String::from_utf8_lossy(other),
            ),
        ),
    }
}

fn xgroup_create<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    // XGROUP CREATE key group <id|$> [MKSTREAM]
    if !(5..=6).contains(&args.len()) {
        return wrong_args(out, "xgroup|create");
    }
    let key = &args[2];
    let group = &args[3];
    let mode = match parse_id_or_dollar(&args[4]) {
        Ok(m) => m,
        Err(msg) => return encode_error(out, msg),
    };
    let mkstream = args.len() == 6 && args[5].eq_ignore_ascii_case(b"MKSTREAM");
    match store.xgroup_create(key, group, mode, mkstream) {
        Ok(true) => encode_simple_string(out, "OK"),
        Ok(false) => encode_error(
            out,
            "BUSYGROUP Consumer Group name already exists",
        ),
        Err(kevy_store::StoreError::NoSuchKey) => encode_error(
            out,
            "ERR The XGROUP subcommand requires the key to exist. \
             Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically.",
        ),
        Err(e) => store_err(out, e),
    }
}

fn xgroup_destroy<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 4 {
        return wrong_args(out, "xgroup|destroy");
    }
    match store.xgroup_destroy(&args[2], &args[3]) {
        Ok(true) => encode_integer(out, 1),
        Ok(false) => encode_integer(out, 0),
        Err(e) => store_err(out, e),
    }
}

fn xgroup_setid<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 5 {
        return wrong_args(out, "xgroup|setid");
    }
    let mode = match parse_id_or_dollar(&args[4]) {
        Ok(m) => m,
        Err(msg) => return encode_error(out, msg),
    };
    match store.xgroup_setid(&args[2], &args[3], mode) {
        Ok(true) => encode_simple_string(out, "OK"),
        Ok(false) => encode_error(out, "NOGROUP No such consumer group"),
        Err(e) => store_err(out, e),
    }
}

fn xgroup_create_consumer<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 5 {
        return wrong_args(out, "xgroup|createconsumer");
    }
    match store.xgroup_create_consumer(&args[2], &args[3], &args[4], now_unix_ms()) {
        Ok(true) => encode_integer(out, 1),
        Ok(false) => encode_integer(out, 0),
        Err(e) => store_err(out, e),
    }
}

fn xgroup_del_consumer<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() != 5 {
        return wrong_args(out, "xgroup|delconsumer");
    }
    match store.xgroup_del_consumer(&args[2], &args[3], &args[4]) {
        Ok(n) => encode_integer(out, n as i64),
        Err(e) => store_err(out, e),
    }
}

fn parse_id_or_dollar(s: &[u8]) -> Result<GroupCreateMode, &'static str> {
    if s == b"$" {
        return Ok(GroupCreateMode::AtCurrent);
    }
    parse_explicit_id(s, /*end=*/ false)
        .map(GroupCreateMode::AtId)
        .map_err(|_| "ERR Invalid stream ID specified as stream command argument")
}

// ───────────── XREADGROUP ─────────────

/// `XREADGROUP GROUP g c [COUNT n] [BLOCK ms] [NOACK] STREAMS key [...] id [...]`
pub(super) fn cmd_xreadgroup<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    let parsed = match parse_xreadgroup_argv(args) {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg),
    };
    let mut reply: Vec<super::StreamReply> = Vec::new();
    // BLOCK only takes effect when at least one stream is reading new
    // entries (`>`); a replay-from-PEL form (`XREADGROUP … STREAMS s 0`)
    // returns immediately even with BLOCK set, matching Redis. So we
    // remember "any `>`-stream" before iterating, then check both flags
    // before parking.
    let any_new_stream = parsed.streams.iter().any(|(_, id)| id == b">");
    let blocking = parsed.block_ms.is_some() && any_new_stream;
    for (key, last_seen_arg) in parsed.streams {
        let last_seen = if last_seen_arg == b">" {
            ReadGroupId::New
        } else {
            match parse_explicit_id(&last_seen_arg, /*end=*/ false) {
                Ok(id) => ReadGroupId::ReplayAfter(id),
                Err(_) => {
                    return encode_error(
                        out,
                        "ERR Invalid stream ID specified as stream command argument",
                    );
                }
            }
        };
        let entries = match store.xreadgroup(
            &key,
            &parsed.group,
            &parsed.consumer,
            last_seen,
            parsed.count,
            parsed.noack,
            now_unix_ms(),
        ) {
            Ok(es) => es,
            Err(kevy_store::StoreError::NoSuchKey) => {
                return encode_error(
                    out,
                    &format!(
                        "NOGROUP No such key '{}' or consumer group '{}' in XREADGROUP with GROUP option",
                        String::from_utf8_lossy(&key),
                        String::from_utf8_lossy(&parsed.group),
                    ),
                );
            }
            Err(e) => return store_err(out, e),
        };
        if !entries.is_empty() {
            reply.push((key, entries));
        }
    }
    if reply.is_empty() && blocking {
        // BLOCK + new-mode + nothing fresh → leave out untouched so the
        // dispatcher registers the conn as a waiter on the first stream
        // key. Next XADD on that key wakes us and re-runs xreadgroup.
        return;
    }
    if reply.is_empty() {
        encode_array_len(out, -1);
        return;
    }
    encode_array_len(out, reply.len() as i64);
    for (key, entries) in &reply {
        encode_array_len(out, 2);
        encode_bulk(out, key);
        emit_entries(out, entries);
    }
}

struct XReadGroupParsed {
    group: Vec<u8>,
    consumer: Vec<u8>,
    count: Option<usize>,
    /// `Some(ms)` if `BLOCK ms` was present; v2-7d.4 uses this together
    /// with the "at least one stream reads `>`" check to decide whether
    /// to park the conn when every requested stream is empty.
    block_ms: Option<u64>,
    noack: bool,
    streams: Vec<(Vec<u8>, Vec<u8>)>,
}

fn parse_xreadgroup_argv<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<XReadGroupParsed, &'static str> {
    if args.len() < 7 {
        return Err("ERR wrong number of arguments for 'xreadgroup' command");
    }
    if !args[1].eq_ignore_ascii_case(b"GROUP") {
        return Err("ERR syntax error");
    }
    let group = args[2].to_vec();
    let consumer = args[3].to_vec();
    let mut i = 4;
    let mut count = None;
    let mut block_ms: Option<u64> = None;
    let mut noack = false;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        match tok.as_slice() {
            b"COUNT" => {
                let n = args.get(i + 1).ok_or("ERR syntax error")?;
                let n: usize = std::str::from_utf8(n)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .ok_or("ERR value is not an integer or out of range")?;
                count = Some(n);
                i += 2;
            }
            b"BLOCK" => {
                let ms_arg = args.get(i + 1).ok_or("ERR syntax error")?;
                let ms: u64 = std::str::from_utf8(ms_arg)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .ok_or("ERR timeout is not an integer or out of range")?;
                block_ms = Some(ms);
                i += 2;
            }
            b"NOACK" => {
                noack = true;
                i += 1;
            }
            b"STREAMS" => {
                let rest = args.len() - (i + 1);
                if rest == 0 || !rest.is_multiple_of(2) {
                    return Err(
                        "ERR Unbalanced XREADGROUP list of streams: for each stream key an ID or '>' must be specified.",
                    );
                }
                let n = rest / 2;
                let mut streams = Vec::with_capacity(n);
                for k in 0..n {
                    streams.push((args[i + 1 + k].to_vec(), args[i + 1 + n + k].to_vec()));
                }
                return Ok(XReadGroupParsed {
                    group,
                    consumer,
                    count,
                    block_ms,
                    noack,
                    streams,
                });
            }
            _ => return Err("ERR syntax error"),
        }
    }
    Err("ERR syntax error")
}

// ───────────── XACK ─────────────

pub(super) fn cmd_xack<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 4 {
        return wrong_args(out, "xack");
    }
    let mut ids = Vec::with_capacity(args.len() - 3);
    for i in 3..args.len() {
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
    match store.xack(&args[1], &args[2], &ids) {
        Ok(n) => encode_integer(out, n as i64),
        Err(e) => store_err(out, e),
    }
}

// ───────────── XPENDING ─────────────

pub(super) fn cmd_xpending<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() == 3 {
        match store.xpending_summary(&args[1], &args[2]) {
            Ok(Some(s)) => emit_pending_summary(out, &s),
            Ok(None) => encode_error(out, "NOGROUP No such consumer group"),
            Err(e) => store_err(out, e),
        }
        return;
    }
    let parsed = match parse_xpending_extended(args) {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg),
    };
    match store.xpending_extended(
        &args[1],
        &args[2],
        parsed.idle_min_ms,
        parsed.start,
        parsed.end,
        parsed.count,
        parsed.consumer.as_deref(),
        now_unix_ms(),
    ) {
        Ok(Some(rows)) => emit_pending_extended(out, &rows.rows),
        Ok(None) => encode_error(out, "NOGROUP No such consumer group"),
        Err(e) => store_err(out, e),
    }
}

struct XPendingExtendedArgs {
    idle_min_ms: Option<u64>,
    start: StreamId,
    end: StreamId,
    count: usize,
    consumer: Option<Vec<u8>>,
}

fn parse_xpending_extended<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<XPendingExtendedArgs, &'static str> {
    let mut i = 3;
    let mut idle_min_ms = None;
    if args[i].eq_ignore_ascii_case(b"IDLE") {
        let v = args.get(i + 1).ok_or("ERR syntax error")?;
        idle_min_ms = Some(
            std::str::from_utf8(v)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or("ERR value is not an integer or out of range")?,
        );
        i += 2;
    }
    if args.len() < i + 3 {
        return Err("ERR syntax error");
    }
    let start = parse_range_start(&args[i])
        .map_err(|_| "ERR Invalid stream ID specified as stream command argument")?;
    let end = parse_range_end(&args[i + 1])
        .map_err(|_| "ERR Invalid stream ID specified as stream command argument")?;
    let count: usize = std::str::from_utf8(&args[i + 2])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("ERR value is not an integer or out of range")?;
    i += 3;
    let consumer = if i < args.len() {
        Some(args[i].to_vec())
    } else {
        None
    };
    Ok(XPendingExtendedArgs { idle_min_ms, start, end, count, consumer })
}

fn emit_pending_summary(out: &mut Vec<u8>, s: &kevy_store::PendingSummary) {
    encode_array_len(out, 4);
    encode_integer(out, s.total as i64);
    match s.id_range {
        Some((lo, hi)) => {
            encode_bulk(out, &lo.encode());
            encode_bulk(out, &hi.encode());
        }
        None => {
            encode_null_bulk(out);
            encode_null_bulk(out);
        }
    }
    if s.by_consumer.is_empty() {
        encode_array_len(out, -1);
        return;
    }
    encode_array_len(out, s.by_consumer.len() as i64);
    for (name, n) in &s.by_consumer {
        encode_array_len(out, 2);
        encode_bulk(out, name);
        encode_bulk(out, n.to_string().as_bytes());
    }
}

fn emit_pending_extended(out: &mut Vec<u8>, rows: &[kevy_store::PendingExtendedRow]) {
    encode_array_len(out, rows.len() as i64);
    for r in rows {
        encode_array_len(out, 4);
        encode_bulk(out, &r.id.encode());
        encode_bulk(out, &r.consumer);
        encode_integer(out, r.idle_ms as i64);
        encode_integer(out, r.delivery_count as i64);
    }
}

