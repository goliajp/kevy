//! `XADD` and `XTRIM`. Every argument is read before the key is looked
//! at, so a bad option is refused the same way whatever the key holds.
//!
//! An approximate trim (`~`) removes whole nodes, and where the nodes of a
//! stream fall depends on its history; a replay may hold them elsewhere.
//! So a command that trimmed approximately is recorded as the exact trim
//! it made (`MAXLEN = <length it left>`), or without a trim when it made
//! none.

use kevy_resp::{ArgvView, CmdError, encode_bulk, encode_error, encode_integer, encode_null_bulk};
use kevy_store::{
    MissingStream, Store, StreamId, TrimMode, TrimRefs, TrimTo, Trimmed, XAddIdSpec, now_unix_ms,
    parse_xadd_id,
};

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
    let fields = (parsed.id_at + 1..args.len()).step_by(2).map(|i| (&args[i], &args[i + 1]));
    let id = match store.xadd_from(&args[1], parsed.id, fields, parsed.missing, now_unix_ms()) {
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
    match (parsed.trim, trimmed) {
        (Some(t), Some(done)) if t.mode != TrimMode::Exact && t.refs != TrimRefs::KeepRef => {
            let kept = stream_len(store, &args[1]);
            Effect::Record(add_record(args, parsed.id_at, id, trim_record(&t, &done, kept)))
        }
        (Some(t), Some(done)) if t.mode != TrimMode::Exact => {
            let kept = if done.removed > 0 { stream_len(store, &args[1]) } else { u64::MAX };
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
    let wrong = CmdError::Wire("ERR wrong number of arguments for 'xadd' command");
    let id = args.get(i).ok_or(wrong)?;
    let id = parse_xadd_id(id).map_err(|_| CmdError::Wire(BAD_ID))?;
    let trim = trim.finish()?;
    let rest = args.len() - i - 1;
    if rest == 0 || !rest.is_multiple_of(2) {
        return Err(wrong);
    }
    if id == XAddIdSpec::Explicit(StreamId::MIN) {
        return Err(CmdError::Wire("ERR The ID specified in XADD must be greater than 0-0"));
    }
    Ok(XAddParsed { missing, id_at: i, trim, id })
}

fn trim(store: &mut Store, key: &[u8], t: Trim) -> Trimmed {
    store.xtrim_refs(key, t.to, t.mode, t.refs).unwrap_or_default()
}

/// The exact trim an approximate one made, to record in its place:
/// `MAXLEN = <length it left>` (with `DELREF` when it dropped
/// references), and for `ACKED` the trim itself made exact, or a `MINID`
/// at the node its `LIMIT` stopped at. Nothing when it removed nothing.
fn trim_record(t: &Trim, done: &Trimmed, kept: u64) -> Vec<Vec<u8>> {
    if done.removed == 0 {
        return Vec::new();
    }
    let id = |id: StreamId| crate::aof::id_bytes(&mut [0u8; 41], id).to_vec();
    let word = |w: &str| w.as_bytes().to_vec();
    match (t.refs, done.cut_at, t.to) {
        (TrimRefs::Acked, Some(at), _) => vec![word("MINID"), word("="), id(at), word("ACKED")],
        (TrimRefs::Acked, None, TrimTo::MaxLen(n)) => {
            vec![word("MAXLEN"), word("="), n.to_string().into_bytes(), word("ACKED")]
        }
        (TrimRefs::Acked, None, TrimTo::MinId(m)) => {
            vec![word("MINID"), word("="), id(m), word("ACKED")]
        }
        (TrimRefs::DelRef, ..) => {
            vec![word("MAXLEN"), word("="), kept.to_string().into_bytes(), word("DELREF")]
        }
        _ => vec![word("MAXLEN"), word("="), kept.to_string().into_bytes()],
    }
}

/// `XADD key [NOMKSTREAM] <trim> <the ID it gave> field value ...`.
fn add_record<A: ArgvView + ?Sized>(
    args: &A,
    id_at: usize,
    id: StreamId,
    trim: Vec<Vec<u8>>,
) -> Vec<Vec<u8>> {
    let mut f = vec![args[0].to_vec(), args[1].to_vec()];
    if (2..id_at).any(|i| args[i].eq_ignore_ascii_case(b"NOMKSTREAM")) {
        f.push(b"NOMKSTREAM".to_vec());
    }
    f.extend(trim);
    f.push(crate::aof::id_bytes(&mut [0u8; 41], id).to_vec());
    f.extend((id_at + 1..args.len()).map(|i| args[i].to_vec()));
    f
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
    let t = match parse_xtrim_argv(args) {
        Ok(t) => t,
        Err(e) => {
            encode_error(out, e.as_wire());
            return Effect::Write;
        }
    };
    let done = match store.xtrim_refs(&args[1], t.to, t.mode, t.refs) {
        Ok(d) => d,
        Err(e) => {
            store_err(out, e);
            return Effect::Write;
        }
    };
    encode_integer(out, done.removed as i64);
    match (t.mode != TrimMode::Exact, done.removed) {
        (_, 0) => Effect::Unchanged,
        (true, _) => {
            let kept = stream_len(store, &args[1]);
            let mut frame = vec![b"XTRIM".to_vec(), args[1].to_vec()];
            frame.extend(trim_record(&t, &done, kept));
            Effect::Record(frame)
        }
        (false, _) => Effect::Write,
    }
}

/// XTRIM's options, every argument after the key one of them.
fn parse_xtrim_argv<A: ArgvView + ?Sized>(args: &A) -> Result<Trim, CmdError> {
    let syntax = CmdError::Wire("ERR syntax error");
    let mut parser = TrimParser::default();
    let mut i = 2;
    while i < args.len() {
        i += parser.take(args, i)?.ok_or(syntax)?;
    }
    parser.finish()?.ok_or(syntax)
}
