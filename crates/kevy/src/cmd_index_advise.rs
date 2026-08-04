//! `IDX.ADVISE` — the read half of the auto-declaration loop's
//! observation face: render the refusal log as executable
//! declarations. A Local dispatch handler like the catalog mutations
//! (the log is process-global origin state; no shard holds anything).

use kevy_index::advice_of;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_error, encode_integer};

use crate::state::Ctx;

/// `IDX.ADVISE` — observed refusal families, most-refused first, each
/// as `[count, name, advice]` where advice is the declaration command
/// that would have served the family. Families the table catalog
/// cannot ground (unknown table / column) are withheld: those queries
/// were malformed, not under-declared. After the missing paths come
/// the never-hit ones (count 0, a drop suggestion) — the reclaim
/// face; dropping stays a human act.
pub(crate) fn cmd_idx_advise<A: ArgvView + ?Sized>(ctx: &Ctx<'_>, args: &A, out: &mut Vec<u8>) {
    if args.len() != 1 {
        return encode_error(out, "ERR usage: IDX.ADVISE");
    }
    let table = ctx.state.catalogs.table();
    let entries = ctx.state.catalogs.advise_entries();
    let mut rows = Vec::new();
    if let Some(cat) = table.as_deref() {
        for e in &entries {
            if let Some(advice) = advice_of(e, cat) {
                rows.push((e.count, e.name.clone(), advice));
            }
        }
    }
    let now_s = (kevy_store::now_unix_ms() / 1000) as i64;
    let mut unused: Vec<(Vec<u8>, String)> = ctx
        .state
        .catalogs
        .usage_snapshot()
        .into_iter()
        .filter(|(_, hits, _, _)| *hits == 0)
        .map(|(name, _, _, declared)| {
            let n = String::from_utf8_lossy(&name).into_owned();
            let age = (now_s - declared).max(0);
            (name, format!("IDX.DROP {n}  (never hit in the {age}s since declare)"))
        })
        .collect();
    unused.sort();
    encode_array_len(out, (rows.len() + unused.len()) as i64);
    for (count, name, advice) in rows {
        encode_array_len(out, 3);
        encode_integer(out, count as i64);
        encode_bulk(out, &name);
        encode_bulk(out, advice.as_bytes());
    }
    for (name, advice) in unused {
        encode_array_len(out, 3);
        encode_integer(out, 0);
        encode_bulk(out, &name);
        encode_bulk(out, advice.as_bytes());
    }
}
