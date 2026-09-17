//! `TABLE.DESCRIBE` / `IDX.DESCRIBE` / `VIEW.DESCRIBE` — read a
//! declaration back. Local, like `IDX.ADVISE`: the catalogs are
//! process-global origin state, so no shard is asked. The reply tree is
//! built in `kevy_index` (shared with the embedded dispatch); this file
//! only looks the object up and encodes.

use kevy_index::{Described, describe_index, describe_table, describe_view};
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error};

use crate::state::Ctx;

/// `TABLE.DESCRIBE name`.
pub(crate) fn cmd_table_describe<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: TABLE.DESCRIBE name");
    }
    let tables = ctx.state.catalogs.table();
    match tables.as_deref().and_then(|c| c.get(&args[1])) {
        Some(spec) => encode(out, &describe_table(spec)),
        None => missing(out, "table", &args[1], "TABLE.LIST"),
    }
}

/// `IDX.DESCRIBE name`.
pub(crate) fn cmd_idx_describe<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: IDX.DESCRIBE name");
    }
    let indexes = ctx.state.catalogs.index();
    let tables = ctx.state.catalogs.table();
    match indexes.as_deref().and_then(|c| c.get(&args[1])) {
        Some((spec, _)) => encode(out, &describe_index(spec, tables.iter().flat_map(|c| c.iter()))),
        None => missing(out, "index", &args[1], "IDX.LIST"),
    }
}

/// `VIEW.DESCRIBE name`.
pub(crate) fn cmd_view_describe<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 2 {
        return encode_error(out, "ERR usage: VIEW.DESCRIBE name");
    }
    let views = ctx.state.catalogs.view();
    match views.as_deref().and_then(|c| c.get(&args[1])) {
        Some(spec) => encode(out, &describe_view(spec)),
        None => missing(out, "view", &args[1], "VIEW.LIST"),
    }
}

fn missing(out: &mut Vec<u8>, noun: &str, name: &[u8], lister: &str) {
    let name = String::from_utf8_lossy(name);
    encode_error(out, &format!("ERR no such {noun} '{name}' ({lister} enumerates them)"));
}

fn encode(out: &mut Vec<u8>, d: &Described) {
    match d {
        Described::Bulk(b) => encode_bulk(out, b),
        Described::Array(items) => {
            encode_array_len(out, items.len() as i64);
            for item in items {
                encode(out, item);
            }
        }
    }
}
