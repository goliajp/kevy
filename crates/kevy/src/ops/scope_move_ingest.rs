//! `MOVE-SCOPE-INGEST` — the target side of a scope move.
//!
//! Split from `scope_move` by direction: that file ships a prefix out,
//! this one takes one in. They fail differently and it is worth them
//! reading differently — a shipper that cannot connect leaves the data
//! where it is, while a receiver that cannot APPLY a frame must say so
//! loudly enough that the shipper does not commit ownership.

use kevy_resp::{ArgvView, encode_error, parse_command};
use kevy_store::Store;

use crate::state::Ctx;

/// `MOVE-SCOPE-INGEST <prefix> <bulk>` — target-side receiver.
/// Parses concatenated RESP commands out of `<bulk>` and dispatches
/// each one with scope routing bypassed for `<prefix>`.
pub(crate) fn cmd_move_scope_ingest<A: ArgvView + ?Sized>(
    ctx: &Ctx<'_>,
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    if args.len() != 3 {
        return encode_error(
            out,
            "ERR wrong number of arguments — MOVE-SCOPE-INGEST <prefix> <bulk>",
        );
    }
    let Some(prefix) = args.get(1) else {
        return encode_error(out, "ERR MOVE-SCOPE-INGEST: missing prefix");
    };
    let Some(bulk) = args.get(2) else {
        return encode_error(out, "ERR MOVE-SCOPE-INGEST: missing bulk");
    };

    let _guard = ctx.shard.ingest_guard(prefix.to_vec());
    let applied = match apply_ingest_frames(ctx, store, bulk) {
        Ok(n) => n,
        Err(IngestError::Malformed) => {
            return encode_error(out, "ERR MOVE-SCOPE-INGEST: malformed bulk");
        }
        // A refused frame means this prefix did NOT arrive intact. The
        // source aborts on an error reply, which leaves the data where
        // it is; answering +OK would hand ownership to a node missing
        // the key, and the row would be unreachable from either side.
        Err(IngestError::Refused(why)) => {
            return encode_error(out, &format!("ERR MOVE-SCOPE-INGEST: {why}"));
        }
    };
    let reply = format!("+OK {applied}\r\n");
    out.extend_from_slice(reply.as_bytes());
}


/// Replay the shipped frames into `store`, returning how many applied.
///
/// The rows land by replaying frames straight in, which bypasses the
/// write path's derived-structure hook — an ingested row used to exist
/// in the keyspace and be invisible to every `IDX.QUERY`, with
/// `IDX.VERIFY` unable to see that direction either. Every emitted
/// frame is `<VERB> <key> …` by construction (`scope_move_emit`), so
/// the key is `argv[1]`.
/// Why an ingest could not complete.
pub(crate) enum IngestError {
    /// The bulk did not parse as RESP commands.
    Malformed,
    /// The target refused a frame — carries the key and the reply, so
    /// the operator learns which row and why rather than "it failed".
    Refused(String),
}

fn apply_ingest_frames(
    ctx: &Ctx<'_>,
    store: &mut Store,
    bulk: &[u8],
) -> Result<usize, IngestError> {
    let mut buf = bulk.to_vec();
    let mut applied = 0usize;
    let mut scratch = Vec::with_capacity(256);
    loop {
        match parse_command(&buf) {
            Ok(Some((argv, consumed))) => {
                scratch.clear();
                crate::dispatch::dispatch_into(ctx, store, &argv, &mut scratch);
                // The reply says whether the frame applied. Ignoring it
                // counted a refusal as a success, the source committed
                // on that count, and the row ended up unreachable from
                // both nodes.
                if scratch.first() == Some(&b'-') {
                    return Err(IngestError::Refused(refusal_detail(&argv, &scratch)));
                }
                if let Some(key) = argv.get(1) {
                    note_ingested_key(ctx, store, key);
                }
                buf.drain(..consumed);
                applied += 1;
            }
            Ok(None) => return Ok(applied),
            Err(_) => return Err(IngestError::Malformed),
        }
    }
}

/// "key 'app:x' refused: WRONGTYPE …" — the key first, because that is
/// what an operator has to go look at.
fn refusal_detail(argv: &kevy_resp::Argv, reply: &[u8]) -> String {
    let key = argv.get(1).map(|k| String::from_utf8_lossy(k).into_owned());
    let msg = String::from_utf8_lossy(reply);
    let msg = msg.trim_start_matches('-').trim_end().to_string();
    match key {
        Some(k) => format!("key '{k}' refused by this node: {msg}"),
        None => format!("a frame was refused by this node: {msg}"),
    }
}

/// Refresh the derived structures for one ingested key — the
/// `Commands::on_write` body, reachable from the ops layer.
fn note_ingested_key(ctx: &Ctx<'_>, store: &mut Store, key: &[u8]) {
    if ctx.state.catalogs.index_nonempty() {
        crate::index_runtime::on_write(ctx, store, key);
    }
    if ctx.state.catalogs.view_nonempty() {
        crate::view_runtime::on_write(ctx, store, key);
    }
}
