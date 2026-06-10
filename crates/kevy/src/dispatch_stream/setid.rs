//! `XSETID key last-id [ENTRIESADDED n] [MAXDELETEDID id]` — overwrite a
//! stream's scalar state (Redis 7 shape). The AOF rewrite leans on this
//! to restore `last_id` / `entries_added` / `max_deleted_id` exactly when
//! a bare XADD replay wouldn't (deleted tail, deleted-only stream).

use kevy_resp::{ArgvView, encode_error, encode_simple_string};
use kevy_store::{Store, StreamId, parse_explicit_id};

use crate::cmd::{store_err, wrong_args};

pub(super) fn cmd_xsetid<A: ArgvView + ?Sized>(store: &mut Store, args: &A, out: &mut Vec<u8>) {
    if !matches!(args.len(), 3 | 5 | 7) {
        return wrong_args(out, "xsetid");
    }
    let last_id = match parse_id(&args[2]) {
        Ok(id) => id,
        Err(msg) => return encode_error(out, msg),
    };
    let mut entries_added: Option<u64> = None;
    let mut max_deleted_id: Option<StreamId> = None;
    let mut i = 3;
    while i < args.len() {
        let tok = args[i].to_ascii_uppercase();
        match tok.as_slice() {
            b"ENTRIESADDED" => {
                let n = std::str::from_utf8(&args[i + 1])
                    .ok()
                    .and_then(|s| s.parse().ok());
                let Some(n) = n else {
                    return encode_error(out, "ERR value is not an integer or out of range");
                };
                entries_added = Some(n);
            }
            b"MAXDELETEDID" => {
                max_deleted_id = match parse_id(&args[i + 1]) {
                    Ok(id) => Some(id),
                    Err(msg) => return encode_error(out, msg),
                };
            }
            _ => return encode_error(out, "ERR syntax error"),
        }
        i += 2;
    }
    match store.xsetid(&args[1], last_id, entries_added, max_deleted_id) {
        Ok(()) => encode_simple_string(out, "OK"),
        Err(kevy_store::StoreError::NoSuchKey) => encode_error(
            out,
            "ERR The XSETID command requires the key to exist.",
        ),
        Err(kevy_store::StoreError::OutOfRange) => encode_error(
            out,
            "ERR The ID specified in XSETID is smaller than the target stream top item",
        ),
        Err(e) => store_err(out, e),
    }
}

fn parse_id(s: &[u8]) -> Result<StreamId, &'static str> {
    parse_explicit_id(s, /*end=*/ false)
        .map_err(|_| "ERR Invalid stream ID specified as stream command argument")
}
