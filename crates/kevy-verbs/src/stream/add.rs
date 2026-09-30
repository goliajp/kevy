//! `XADD` and `XTRIM`. Every argument is read before the key is looked
//! at, so a bad option is refused the same way whatever the key holds.
//!
//! An approximate trim (`~`) removes whole nodes, and where the nodes of a
//! stream fall depends on its history; a replay may hold them elsewhere.
//! So a command that trimmed approximately is recorded as the exact trim
//! it made (`MAXLEN = <length it left>`), or without a trim when it made
//! none.

use kevy_resp::{ArgvView, CmdError, encode_bulk, encode_error, encode_integer, encode_null_bulk};
use kevy_store::{MissingStream, Store, StreamId, XAddIdSpec, now_unix_ms, parse_xadd_id};

use super::opts::{BAD_ID, Trim, TrimParser};
use crate::Effect;
use crate::reply::{store_err, wrong_args};

/// `XADD key [NOMKSTREAM] [MAXLEN|MINID [=|~] threshold [LIMIT n]] <id|*>
/// field value [field value ...]`
///
/// An ID the command generated (`*`, `ms-*`) is recorded as the ID it
/// gave ([`Effect::RecordId`]), every other argument as it came.
pub(super) fn cmd_xadd<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 5 {
        wrong_args(out, "xadd");
        return Effect::Write;
    }
    let parsed = match parse_xadd_argv(args) {
        Ok(p) => p,
        Err(msg) => {
            encode_error(out, msg.as_wire());
            return Effect::Write;
        }
    };
    let fields: Vec<(Vec<u8>, Vec<u8>)> = (parsed.id_at + 1..args.len())
        .step_by(2)
        .map(|i| (args[i].to_vec(), args[i + 1].to_vec()))
        .collect();
    let id = match store.xadd(&args[1], parsed.id, fields, parsed.missing, now_unix_ms()) {
        Ok(Some(id)) => id,
        Ok(None) => {
            encode_null_bulk(out); // NOMKSTREAM + missing key
            return Effect::Unchanged;
        }
        Err(e) => {
            xadd_err(out, e);
            return Effect::Write;
        }
    };
    let trimmed = parsed.trim.map(|t| trim(store, &args[1], t));
    encode_bulk(out, crate::aof::id_bytes(&mut [0u8; 41], id));
    let generated = !matches!(parsed.id, XAddIdSpec::Explicit(_));
    match parsed.trim {
        Some(t) if t.approx => {
            let kept = trimmed.filter(|n| *n > 0).map_or(u64::MAX, |_| stream_len(store, &args[1]));
            Effect::RecordAdd(parsed.id_at, id, kept)
        }
        _ if generated => Effect::RecordId(parsed.id_at, id),
        _ => Effect::Write,
    }
}

fn xadd_err(out: &mut Vec<u8>, e: kevy_store::StoreError) {
    match e {
        kevy_store::StoreError::OutOfRange => encode_error(
            out,
            "ERR The ID specified in XADD is equal or smaller than the target stream top item",
        ),
        e => store_err(out, e),
    }
}

struct XAddParsed {
    missing: MissingStream,
    /// Where the ID argument sits.
    id_at: usize,
    trim: Option<Trim>,
    id: XAddIdSpec,
}

fn parse_xadd_argv<A: ArgvView + ?Sized>(args: &A) -> Result<XAddParsed, CmdError> {
    let mut i = 2;
    let mut missing = MissingStream::Create;
    let mut trim = TrimParser::default();
    while i < args.len() {
        if args[i].eq_ignore_ascii_case(b"NOMKSTREAM") {
            missing = MissingStream::Refuse;
            i += 1;
            continue;
        }
        match trim.take(args, i)? {
            Some(n) => i += n,
            None => break,
        }
    }
    let rest = args.len().saturating_sub(i + 1);
    if rest == 0 || !rest.is_multiple_of(2) {
        return Err(CmdError::Wire("ERR wrong number of arguments for 'xadd' command"));
    }
    let trim = trim.finish()?;
    let id = parse_xadd_id(&args[i]).map_err(|_| CmdError::Wire(BAD_ID))?;
    if id == XAddIdSpec::Explicit(StreamId::MIN) {
        return Err(CmdError::Wire("ERR The ID specified in XADD must be greater than 0-0"));
    }
    Ok(XAddParsed { missing, id_at: i, trim, id })
}

fn trim(store: &mut Store, key: &[u8], t: Trim) -> u64 {
    store.xtrim(key, t.to, t.approx, t.limit).unwrap_or(0)
}

fn stream_len(store: &mut Store, key: &[u8]) -> u64 {
    store.xlen(key).unwrap_or(0)
}

/// `XTRIM key MAXLEN|MINID [=|~] threshold [LIMIT n]`
pub(super) fn cmd_xtrim<A: ArgvView + ?Sized>(
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) -> Effect {
    if args.len() < 4 {
        wrong_args(out, "xtrim");
        return Effect::Write;
    }
    let mut parser = TrimParser::default();
    let mut i = 2;
    while i < args.len() {
        match parser.take(args, i) {
            Ok(Some(n)) => i += n,
            Ok(None) => {
                encode_error(out, "ERR syntax error");
                return Effect::Write;
            }
            Err(e) => {
                encode_error(out, e.as_wire());
                return Effect::Write;
            }
        }
    }
    let t = match parser.finish() {
        Ok(Some(t)) => t,
        Ok(None) => {
            encode_error(out, "ERR syntax error");
            return Effect::Write;
        }
        Err(e) => {
            encode_error(out, e.as_wire());
            return Effect::Write;
        }
    };
    let n = match store.xtrim(&args[1], t.to, t.approx, t.limit) {
        Ok(n) => n,
        Err(e) => {
            store_err(out, e);
            return Effect::Write;
        }
    };
    encode_integer(out, n as i64);
    match (t.approx, n) {
        (_, 0) => Effect::Unchanged,
        (true, _) => {
            let kept = stream_len(store, &args[1]).to_string().into_bytes();
            Effect::Record(vec![
                b"XTRIM".to_vec(),
                args[1].to_vec(),
                b"MAXLEN".to_vec(),
                b"=".to_vec(),
                kept,
            ])
        }
        (false, _) => Effect::Write,
    }
}
