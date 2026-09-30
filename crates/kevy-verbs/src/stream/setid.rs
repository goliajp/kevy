//! `XSETID key last-id [ENTRIESADDED n] [MAXDELETEDID id]` — overwrite a
//! stream's scalar state (Redis 7 shape). The AOF rewrite leans on this
//! to restore `last_id` / `entries_added` / `max_deleted_id` exactly when
//! a bare XADD replay wouldn't (deleted tail, deleted-only stream).

use kevy_resp::CmdError;
use kevy_resp::{ArgvView, encode_error, encode_simple_string};
use kevy_store::{Store, StreamId, parse_explicit_id};

use crate::reply::{store_err, wrong_args};

/// Every argument is read first; then, on the stream, an ID below the
/// `MAXDELETEDID` given, below the stream's own, or below its last entry
/// is refused, and so is an `ENTRIESADDED` below its length.
pub(super) fn cmd_xsetid<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if args.len() < 3 {
        return wrong_args(out, "xsetid");
    }
    let (last_id, added, deleted) = match parse(args) {
        Ok(p) => p,
        Err(msg) => return encode_error(out, msg.as_wire()),
    };
    let s = match store.stream_view(&args[1]) {
        Ok(Some(s)) => s,
        Ok(None) => return encode_error(out, "ERR no such key"),
        Err(e) => return store_err(out, e),
    };
    let refusal = if deleted.is_some_and(|d| last_id < d) {
        Some("ERR The ID specified in XSETID is smaller than the provided max_deleted_entry_id")
    } else if last_id < s.max_deleted_id() {
        Some("ERR The ID specified in XSETID is smaller than current max_deleted_entry_id")
    } else if s.last_entry().is_some_and(|(top, _)| last_id < top) {
        Some("ERR The ID specified in XSETID is smaller than the target stream top item")
    } else if added.is_some_and(|n| n < s.length()) {
        Some("ERR The entries_added specified in XSETID is smaller than the target stream length")
    } else {
        None
    };
    if let Some(msg) = refusal {
        return encode_error(out, msg);
    }
    match store.xsetid(&args[1], last_id, added, deleted) {
        Ok(()) => encode_simple_string(out, "OK"),
        Err(e) => store_err(out, e),
    }
}

/// `last-id [ENTRIESADDED n] [MAXDELETEDID id]`, each option as often as
/// given, the last one kept.
fn parse<A: ArgvView + ?Sized>(
    args: &A,
) -> Result<(StreamId, Option<u64>, Option<StreamId>), CmdError> {
    let last_id = parse_id(&args[2])?;
    let (mut added, mut deleted) = (None, None);
    let mut i = 3;
    while i < args.len() {
        let v = args.get(i + 1).ok_or(CmdError::Wire("ERR syntax error"))?;
        if args[i].eq_ignore_ascii_case(b"ENTRIESADDED") {
            let n = super::opts::strict_i64(v).ok_or(CmdError::Wire(crate::reply::ERR_NOT_INT))?;
            added = Some(
                u64::try_from(n)
                    .map_err(|_| CmdError::Wire("ERR entries_added must be positive"))?,
            );
        } else if args[i].eq_ignore_ascii_case(b"MAXDELETEDID") {
            deleted = Some(parse_id(v)?);
        } else {
            return Err(CmdError::Wire("ERR syntax error"));
        }
        i += 2;
    }
    Ok((last_id, added, deleted))
}

fn parse_id(s: &[u8]) -> Result<StreamId, CmdError> {
    parse_explicit_id(s)
        .map_err(|_| CmdError::Wire("ERR Invalid stream ID specified as stream command argument"))
}
