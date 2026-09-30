//! `XREAD [COUNT n] [BLOCK ms] STREAMS key… id…`.
//!
//! Each stream is checked, its key and its ID, before any is read. An ID
//! is an explicit one (read what follows it), `$` (what follows the last
//! entry: nothing, until the stream grows) or `+` (the last entry itself).
//! A `COUNT` of zero or less reads everything.
//!
//! BLOCK with nothing to answer writes no reply at all: the runtime parks
//! the connection on the first stream key, and the next `XADD` there runs
//! the command again. Multi-stream BLOCK across shards is constrained by
//! the routing layer (only the first STREAMS key drives shard selection).

use kevy_resp::CmdError;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error};
use kevy_store::{EntryBatch, Store, StreamId, parse_explicit_id};

use crate::reply::{store_err, wrong_args};

use super::opts::{BAD_ID, strict_i64};
use super::{StreamReply, emit_entries};

pub(super) fn cmd_xread<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 4 {
        return wrong_args(out, "xread");
    }
    let parsed = match parse_xread_argv(args) {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    let mut froms = Vec::with_capacity(parsed.streams);
    for k in 0..parsed.streams {
        match check_stream(store, &args[parsed.keys + k], &args[parsed.keys + parsed.streams + k]) {
            Ok(from) => froms.push(from),
            Err(e) => return e.emit(out),
        }
    }
    let mut reply: Vec<StreamReply> = Vec::new();
    for (k, from) in froms.into_iter().enumerate() {
        let key = &args[parsed.keys + k];
        let entries = match from {
            From::After(id) => store.xread(key, id, parsed.count),
            From::Last => last_entry(store, key),
        };
        match entries {
            Ok(es) if !es.is_empty() => reply.push((key.to_vec(), es)),
            Ok(_) => {}
            Err(e) => return store_err(out, e),
        }
    }
    if reply.is_empty() && parsed.block {
        return;
    }
    emit_xread_reply(out, &reply);
}

/// Where a stream is read from.
enum From {
    After(StreamId),
    Last,
}

enum Refusal {
    Store(kevy_store::StoreError),
    Wire(&'static str),
}

impl Refusal {
    fn emit(self, out: &mut Vec<u8>) {
        match self {
            Refusal::Store(e) => store_err(out, e),
            Refusal::Wire(m) => encode_error(out, m),
        }
    }
}

/// The key's type, then the ID.
fn check_stream(store: &mut Store, key: &[u8], id: &[u8]) -> Result<From, Refusal> {
    store.stream_view(key).map_err(Refusal::Store)?;
    match id {
        b"$" => store.xread_dollar_last_id(key).map(From::After).map_err(Refusal::Store),
        b"+" => Ok(From::Last),
        b">" => Err(Refusal::Wire(
            "ERR The > ID can be specified only when calling XREADGROUP using the GROUP <group> <consumer> option.",
        )),
        _ => parse_explicit_id(id).map(From::After).map_err(|_| Refusal::Wire(BAD_ID)),
    }
}

/// The last entry, when there is one.
fn last_entry(store: &mut Store, key: &[u8]) -> Result<EntryBatch, kevy_store::StoreError> {
    let Some(s) = store.stream_view(key)? else { return Ok(Vec::new()) };
    Ok(s.last_entry()
        .map(|(id, fv)| vec![(id, fv.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect())])
        .unwrap_or_default())
}

struct XReadParsed {
    count: Option<usize>,
    block: bool,
    keys: usize,
    streams: usize,
}

fn parse_xread_argv<A: ArgvView + ?Sized>(args: &A) -> Result<XReadParsed, CmdError> {
    let mut p = XReadParsed { count: None, block: false, keys: 0, streams: 0 };
    let mut i = 1;
    while i < args.len() {
        let tok = &args[i];
        if tok.eq_ignore_ascii_case(b"COUNT") {
            let n = args.get(i + 1).ok_or(CmdError::Wire("ERR syntax error"))?;
            let n = strict_i64(n).ok_or(CmdError::Wire(crate::reply::ERR_NOT_INT))?;
            p.count = usize::try_from(n).ok().filter(|n| *n > 0);
            i += 2;
        } else if tok.eq_ignore_ascii_case(b"BLOCK") {
            block_ms(args.get(i + 1))?;
            p.block = true;
            i += 2;
        } else if tok.eq_ignore_ascii_case(b"STREAMS") {
            let rest = args.len() - i - 1;
            if rest == 0 || !rest.is_multiple_of(2) {
                return Err(CmdError::Wire(
                    "ERR Unbalanced 'xread' list of streams: for each stream key an ID or '$' must be specified.",
                ));
            }
            p.keys = i + 1;
            p.streams = rest / 2;
            return Ok(p);
        } else {
            return Err(CmdError::Wire("ERR syntax error"));
        }
    }
    Err(CmdError::Wire("ERR syntax error"))
}

/// A `BLOCK` timeout in milliseconds.
pub(super) fn block_ms(arg: Option<&[u8]>) -> Result<u64, CmdError> {
    let v = arg.ok_or(CmdError::Wire("ERR syntax error"))?;
    let n = strict_i64(v).ok_or(CmdError::Wire("ERR timeout is not an integer or out of range"))?;
    u64::try_from(n).map_err(|_| CmdError::Wire("ERR timeout is negative"))
}

fn emit_xread_reply(out: &mut Vec<u8>, reply: &[StreamReply]) {
    if reply.is_empty() {
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
