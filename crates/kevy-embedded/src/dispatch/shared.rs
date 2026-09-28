//! Single-key commands, run by the command layer the server runs them
//! with (`kevy_verbs::exec`): pick the key's shard, take its lock, run
//! the command, record what it changed.
//!
//! What stays out of here, and why: `GET` reads under the shard's shared
//! lock when the eviction policy allows (the facade's lock policy);
//! `DEL`, `UNLINK`, `EXISTS`, `TOUCH`, `MSET`, `RENAME`, `RENAMENX`,
//! `DBSIZE` and `FLUSHALL` span shards. Those keep their facade arms.

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
    let Some(v) = kevy_verbs::verb(up) else {
        return false;
    };
    if SERVER_ONLY.contains(&up) {
        return false;
    }
    run(s, v, up, argv, out);
    true
}

fn run(s: &Store, v: &Verb, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) {
    // a call too short to name a key is refused for its arity alone
    if v.write
        && argv.len() > 1
        && let Err(e) = ensure_writable(s)
    {
        return super::kevy_err(out, &e);
    }
    let key = argv.get(1).map_or(&[][..], Vec::as_slice);
    let mut g = s.wshard(key);
    let mark = out.len();
    let effect = kevy_verbs::exec(&mut g.store, up, &Args(argv), out);
    if out.get(mark) == Some(&b'-') {
        return; // a refused command changed nothing
    }
    let recorded = match effect {
        Some(Effect::Write) => record(&mut g, argv),
        Some(Effect::Record(frame)) => {
            let parts: Vec<&[u8]> = frame.iter().map(Vec::as_slice).collect();
            commit_write(&mut g, &parts)
        }
        _ => Ok(()),
    };
    if let Err(e) = recorded {
        out.truncate(mark);
        super::kevy_err(out, &e);
    }
}

/// Record a write as the argv it ran with, then the absolute deadline
/// it set when it moved one by a relative amount.
fn record(g: &mut Inner, argv: &[Vec<u8>]) -> KevyResult<()> {
    // the common short argv is viewed from the stack: logging a write
    // allocates nothing on the calling thread
    const INLINE: usize = 8;
    if argv.len() <= INLINE {
        let mut parts: [&[u8]; INLINE] = [&[]; INLINE];
        for (slot, a) in parts.iter_mut().zip(argv) {
            *slot = a;
        }
        commit_write(g, &parts[..argv.len()])?;
    } else {
        let parts: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
        commit_write(g, &parts)?;
    }
    for f in kevy_verbs::aof::ttl_followup(&mut g.store, &Args(argv)) {
        let parts: Vec<&[u8]> = (0..f.len()).map(|i| &f[i]).collect();
        commit_write(g, &parts)?;
    }
    Ok(())
}
