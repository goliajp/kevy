//! Single-key commands, run by the command layer the server runs them
//! with (`kevy_verbs::exec`): pick the key's shard, take its lock, run
//! the command, record what it changed.
//!
//! What stays out of here, and why: `GET` reads under the shard's shared
//! lock when the eviction policy allows (the facade's lock policy);
//! `DEL`, `UNLINK`, `EXISTS`, `TOUCH`, `MSET`, `RENAME`, `RENAMENX`,
//! `DBSIZE` and `FLUSHALL` span shards. Those keep their facade arms.
//!
//! The stream and geo commands are served with the `streams-geo`
//! feature. A stream read that asks to block is refused (there is no
//! connection to park), and so is a multi-key stream read or geo store
//! whose keys live on different shards.

use kevy_verbs::{Effect, Verb};

use super::Args;
use crate::KevyResult;
use crate::store::{Inner, Store, commit_write, ensure_writable};

/// Verbs the shared layer runs for the server that this surface does
/// not serve: the blocking pops have no connection to park, the
/// two-key list moves may span shards, and the rest were never part of
/// the embedded surface.
pub(super) const SERVER_ONLY: &[&[u8]] = &[
    b"BLPOP",
    b"BRPOP",
    b"BRPOPLPUSH",
    b"BZPOPMIN",
    b"FLUSHDB",
    b"HMSET",
    b"LMOVE",
    b"LPOS",
    b"PSETEX",
    b"RPOPLPUSH",
    b"SETEX",
    b"SSCAN",
];

/// One single-key command; `false` = the verb is not served here.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    // an internal record verb is applied from a record, never from here
    if [kevy_resp::ops_table::CONSUMER_SEEN, kevy_resp::ops_table::PENDING]
        .iter()
        .any(|v| up == v.as_bytes())
    {
        kevy_resp::encode_error(out, kevy_verbs::aof::INTERNAL_REFUSAL);
        return true;
    }
    let Some(v) = kevy_verbs::verb(up) else {
        return false;
    };
    if SERVER_ONLY.contains(&up) || !cfg!(feature = "streams-geo") && kevy_verbs::is_streams_geo(up)
    {
        return false;
    }
    run(s, v, up, argv, out);
    true
}

fn run(s: &Store, v: &Verb, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) {
    // the dispatcher refused a write on a store that takes none; this
    // catches only a shutdown that landed since
    if v.write
        && let Err(e) = ensure_writable(s)
    {
        return super::kevy_err(out, &e);
    }
    #[cfg(feature = "streams-geo")]
    if let Some(msg) = refusal(s, up, argv) {
        return kevy_resp::encode_error(out, msg);
    }
    let args = Args::new(argv);
    let mut g = s.wshard(crate::verb_keys::shard_key(up, &args).unwrap_or_default());
    let mark = out.len();
    let effect = kevy_verbs::exec(&mut g.store, up, &args, out);
    if out.get(mark) == Some(&b'-') {
        return; // a refused command changed nothing
    }
    let recorded = match effect {
        Some(Effect::Write) => record(&mut g, argv, None),
        Some(Effect::Record(frame)) => {
            let parts: Vec<&[u8]> = frame.iter().map(Vec::as_slice).collect();
            commit_write(&mut g, &parts)
        }
        Some(Effect::RecordId(at, id)) => {
            let mut buf = [0u8; 41];
            let id = kevy_verbs::aof::id_bytes(&mut buf, id);
            record(&mut g, argv, Some((at, id)))
        }
        Some(
            e @ (Effect::RecordClaim(_)
            | Effect::RecordRead(..)
            | Effect::RecordReads(_)
            | Effect::RecordHistory(_)
            | Effect::RecordAdd(..)
            | Effect::RecordSeen),
        ) => record_outcome(&mut g, argv, &e),
        None | Some(Effect::Read | Effect::Unchanged | Effect::Skip) => Ok(()),
    };
    if let Err(e) = recorded {
        out.truncate(mark);
        super::kevy_err(out, &e);
    }
}

/// Why this engine refuses a stream or geo call it would otherwise run:
/// it cannot park a caller, and it runs a command under one shard's lock.
#[cfg(feature = "streams-geo")]
fn refusal(s: &Store, up: &[u8], argv: &[Vec<u8>]) -> Option<&'static str> {
    if !kevy_verbs::is_streams_geo(up) {
        return None;
    }
    let args = Args::new(argv);
    if crate::verb_keys::blocks(up, &args) {
        return Some("ERR the embedded engine cannot block; call without BLOCK");
    }
    let n = s.shards.len();
    let apart = |a: &[u8], b: &[u8]| crate::shard::shard_idx(a, n) != crate::shard::shard_idx(b, n);
    let split = match up {
        _ if n == 1 => false,
        b"XREAD" | b"XREADGROUP" => crate::verb_keys::stream_keys(up, &args)
            .is_some_and(|keys| keys.clone().any(|i| apart(&argv[keys.start], &argv[i]))),
        _ => kevy_verbs::geo::store_keys(up, &args).is_some_and(|(src, dst)| apart(&src, &dst)),
    };
    split.then_some("CROSSSLOT Keys in request don't hash to the same slot")
}

/// Record a write as the argv it ran with, argument `swap.0` replaced by
/// `swap.1` when given, then the absolute deadline it set when it moved
/// one by a relative amount.
fn record(g: &mut Inner, argv: &[Vec<u8>], swap: Option<(usize, &[u8])>) -> KevyResult<()> {
    // the common short argv is viewed from the stack: logging a write
    // allocates nothing on the calling thread
    const INLINE: usize = 8;
    if argv.len() <= INLINE {
        let mut parts: [&[u8]; INLINE] = [&[]; INLINE];
        for (i, (slot, a)) in parts.iter_mut().zip(argv).enumerate() {
            *slot = swapped(swap, i, a);
        }
        commit_write(g, &parts[..argv.len()])?;
    } else {
        let parts: Vec<&[u8]> = argv.iter().enumerate().map(|(i, a)| swapped(swap, i, a)).collect();
        commit_write(g, &parts)?;
    }
    for f in kevy_verbs::aof::ttl_followup(&g.store, &Args::new(argv)) {
        let parts: Vec<&[u8]> = (0..f.len()).map(|i| &f[i]).collect();
        commit_write(g, &parts)?;
    }
    Ok(())
}

/// Argument `i` of a record: `a`, or the replacement `swap` names for it.
fn swapped<'a>(swap: Option<(usize, &'a [u8])>, i: usize, a: &'a [u8]) -> &'a [u8] {
    match swap {
        Some((at, v)) if at == i => v,
        _ => a,
    }
}

/// Record a claim, a group read or a new consumer as its outcome where the write is
/// recorded (the AOF, a replica source, the change feed); elsewhere the
/// argv runs the commit's other steps, and no frame is built.
fn record_outcome(g: &mut Inner, argv: &[Vec<u8>], outcome: &Effect) -> KevyResult<()> {
    if !crate::store_glue::records_writes(g) {
        return record(g, argv, None);
    }
    for f in kevy_verbs::aof::deferred_frames(&g.store, &Args::new(argv), outcome) {
        let parts: Vec<&[u8]> = (0..f.len()).map(|i| &f[i]).collect();
        commit_write(g, &parts)?;
    }
    Ok(())
}
