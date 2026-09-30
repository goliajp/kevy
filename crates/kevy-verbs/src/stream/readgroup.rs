//! `XREADGROUP GROUP g c [COUNT n] [BLOCK ms] [NOACK] STREAMS key… id…`.
//!
//! Every stream is checked, its key, group and ID, before any is read, so
//! a refused command reads nothing. A stream read with `>` that has
//! nothing new is left out of the reply; a read of a consumer's history
//! (an explicit ID) is always in it, empty or not, and hands back an entry
//! the stream no longer holds with no fields. Every entry a history read
//! hands back counts as delivered again.

use kevy_resp::{ArgvView, CmdError, encode_array_len, encode_bulk, encode_error};
use kevy_store::{
    AckMode, GroupBatch, ReadGroupId, Store, StoreError, now_unix_ms, parse_explicit_id,
};

use super::claim_record::ReadMarks;
use super::opts::{BAD_ID, strict_i64};
use crate::Effect;
use crate::reply::store_err;

const PLUS: &str = "ERR The + ID is meaningless in the context of XREADGROUP: you want to read \
     the history of this consumer by specifying a proper ID, or use the > ID to get new messages. \
     The + ID would just return an empty result set.";
const DOLLAR: &str = "ERR The $ ID is meaningless in the context of XREADGROUP: you want to read \
     the history of this consumer by specifying a proper ID, or use the > ID to get new messages. \
     The $ ID would just return an empty result set.";

pub(super) fn cmd_xreadgroup<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Effect {
    let parsed = match parse(args) {
        Ok(p) => p,
        Err(msg) => {
            encode_error(out, msg.as_wire());
            return Effect::Write;
        }
    };
    let reads: Result<Vec<ReadGroupId>, String> =
        (0..parsed.streams).map(|k| check_stream(store, args, &parsed, k)).collect();
    let reads = match reads {
        Ok(reads) => reads,
        Err(e) => {
            encode_error(out, &e);
            return Effect::Write;
        }
    };
    let blocking = parsed.block && reads.iter().all(|r| *r == ReadGroupId::New);
    let (reply, marks) = match read_all(store, args, &parsed, reads) {
        Ok(read) => read,
        Err(e) => {
            store_err(out, e);
            return Effect::Write;
        }
    };
    if !(reply.is_empty() && blocking) {
        emit_reply(out, args, parsed.keys, &reply);
    }
    marks.effect()
}

/// Read every stream from where it was checked to start: the batches to
/// answer, by stream, and what to record.
fn read_all<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    parsed: &Parsed,
    reads: Vec<ReadGroupId>,
) -> Result<(Vec<(usize, GroupBatch)>, ReadMarks), StoreError> {
    let (group, consumer) = (parsed.group(args), parsed.consumer(args));
    let mut reply: Vec<(usize, GroupBatch)> = Vec::new();
    let mut marks = ReadMarks::default();
    let now = now_unix_ms();
    for (k, from) in reads.into_iter().enumerate() {
        let key = &args[parsed.keys + k];
        let mark = ReadMarks::read(store, key, group, consumer);
        let entries =
            store.xreadgroup(key, group, consumer, from, parsed.count, parsed.ack, now)?;
        let history = from != ReadGroupId::New;
        let redelivered = if history {
            entries.iter().filter(|e| e.1.is_some()).map(|e| e.0).collect()
        } else {
            Vec::new()
        };
        marks.push(mark, !history && !entries.is_empty(), redelivered);
        if history || !entries.is_empty() {
            reply.push((k, entries));
        }
    }
    Ok((reply, marks))
}

/// The refusal `XREADGROUP` gives `args` before it reads anything, or
/// `None` when every stream it names checks out. Nothing is read or
/// changed, so a read split across shards can check each part first and
/// read none of them when one would be refused.
///
/// ```
/// use kevy_resp::Argv;
/// let mut store = kevy_store::Store::new();
/// let argv = |s: &str| Argv::from(s.split(' ').map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>());
/// let refusal = kevy_verbs::cmd::xreadgroup_refusal(&mut store, &argv("XREADGROUP GROUP g c STREAMS s >"));
/// assert!(refusal.unwrap().starts_with(b"-NOGROUP"));
/// ```
pub fn xreadgroup_refusal<A: ArgvView + ?Sized>(store: &mut Store, args: &A) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match parse(args) {
        Err(msg) => encode_error(&mut out, msg.as_wire()),
        Ok(p) => {
            if let Some(e) = (0..p.streams).find_map(|k| check_stream(store, args, &p, k).err()) {
                encode_error(&mut out, &e);
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

struct Parsed {
    count: Option<usize>,
    block: bool,
    ack: AckMode,
    /// The first key's index, and how many streams.
    keys: usize,
    streams: usize,
}

impl Parsed {
    fn group<'a, A: ArgvView + ?Sized>(&self, args: &'a A) -> &'a [u8] {
        &args[2]
    }
    fn consumer<'a, A: ArgvView + ?Sized>(&self, args: &'a A) -> &'a [u8] {
        &args[3]
    }
}

fn parse<A: ArgvView + ?Sized>(args: &A) -> Result<Parsed, CmdError> {
    if args.len() < 7 {
        return Err(CmdError::Wire("ERR wrong number of arguments for 'xreadgroup' command"));
    }
    if !args[1].eq_ignore_ascii_case(b"GROUP") {
        return Err(CmdError::Wire("ERR syntax error"));
    }
    let mut p = Parsed { count: None, block: false, ack: AckMode::Pending, keys: 0, streams: 0 };
    let mut i = 4;
    while i < args.len() {
        let tok = &args[i];
        if tok.eq_ignore_ascii_case(b"COUNT") {
            let n = args.get(i + 1).ok_or(CmdError::Wire("ERR syntax error"))?;
            let n = strict_i64(n).ok_or(CmdError::Wire(crate::reply::ERR_NOT_INT))?;
            // zero or less reads everything
            p.count = usize::try_from(n).ok().filter(|n| *n > 0);
            i += 2;
        } else if tok.eq_ignore_ascii_case(b"BLOCK") {
            super::read::block_ms(args.get(i + 1))?;
            p.block = true;
            i += 2;
        } else if tok.eq_ignore_ascii_case(b"NOACK") {
            p.ack = AckMode::NoAck;
            i += 1;
        } else if tok.eq_ignore_ascii_case(b"STREAMS") {
            let rest = args.len() - i - 1;
            if rest == 0 || !rest.is_multiple_of(2) {
                return Err(CmdError::Wire(
                    "ERR Unbalanced 'xreadgroup' list of streams: for each stream key an ID or '>' must be specified.",
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

/// A stream's key and group, then its ID: what it reads from, or the
/// refusal.
fn check_stream<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    p: &Parsed,
    k: usize,
) -> Result<ReadGroupId, String> {
    let (key, id, group) = (&args[p.keys + k], &args[p.keys + p.streams + k], p.group(args));
    match store.stream_view(key) {
        Err(e) => return Err(e.as_wire().to_owned()),
        Ok(Some(s)) if s.group(group).is_some() => {}
        Ok(_) => {
            return Err(format!(
                "NOGROUP No such key '{}' or consumer group '{}' in XREADGROUP with GROUP option",
                String::from_utf8_lossy(key),
                String::from_utf8_lossy(group),
            ));
        }
    }
    match id {
        b">" => Ok(ReadGroupId::New),
        b"+" => Err(PLUS.to_owned()),
        b"$" => Err(DOLLAR.to_owned()),
        _ => parse_explicit_id(id).map(ReadGroupId::ReplayAfter).map_err(|_| BAD_ID.to_owned()),
    }
}

fn emit_reply<A: ArgvView + ?Sized>(
    out: &mut Vec<u8>,
    args: &A,
    keys: usize,
    reply: &[(usize, GroupBatch)],
) {
    if reply.is_empty() {
        encode_array_len(out, -1);
        return;
    }
    encode_array_len(out, reply.len() as i64);
    for (k, entries) in reply {
        encode_array_len(out, 2);
        encode_bulk(out, &args[keys + k]);
        encode_array_len(out, entries.len() as i64);
        for (id, fields) in entries {
            encode_array_len(out, 2);
            encode_bulk(out, crate::aof::id_bytes(&mut [0u8; 41], *id));
            match fields {
                Some(fv) => {
                    encode_array_len(out, (fv.len() * 2) as i64);
                    for (f, v) in fv {
                        encode_bulk(out, f);
                        encode_bulk(out, v);
                    }
                }
                None => encode_array_len(out, -1),
            }
        }
    }
}
