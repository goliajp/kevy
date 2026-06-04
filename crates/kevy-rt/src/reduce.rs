//! Reply reduction and small pure helpers.
//!
//! [`materialize`] turns a completed [`Agg`] accumulator into final RESP bytes;
//! the rest are the stateless pieces used across the runtime — set algebra,
//! pub/sub framing, the seq-ring drain, and the shard hash.

use crate::conn::Conn;
use crate::message::{Agg, Gathered, KeyShape, MultiOp};
use kevy_hash::KevyHash;
use kevy_resp::{
    RespVersion, encode_array_len, encode_bulk, encode_error, encode_integer, encode_null_bulk,
    encode_set_header, encode_simple_string,
};
use std::collections::{HashMap, HashSet};

const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";

/// Turn a completed accumulator into its final RESP reply bytes. The
/// per-conn `proto` flips one or two reply shapes (notably the set-
/// algebra arms of `finalize_gather` — SINTER/SUNION/SDIFF go from
/// `*N` Array to `~N` Set under RESP3). Every other arm is the same
/// bytes on both protos.
pub(crate) fn materialize(agg: Agg, proto: RespVersion) -> Vec<u8> {
    match agg {
        Agg::First(Some(b)) => b,
        Agg::First(None) => {
            let mut out = Vec::new();
            encode_error(&mut out, "ERR internal error");
            out
        }
        Agg::SumInt(n) => {
            let mut out = Vec::new();
            encode_integer(&mut out, n);
            out
        }
        Agg::AllOk => {
            let mut out = Vec::new();
            encode_simple_string(&mut out, "OK");
            out
        }
        Agg::Gather { op, keys, got } => finalize_gather(op, keys, got, proto),
        Agg::Keys { shape, acc } => finalize_keys(shape, acc),
        // WatchCollect / ExecPrep have a conn-state side effect that
        // pure materialise() can't express; `Shard::fold` routes them
        // to `finalize_watch_agg` instead, so they never reach here.
        // Defensive: emit an error rather than silently dropping the
        // slot — a misroute would otherwise hang the connection.
        Agg::WatchCollect { .. } | Agg::ExecPrep { .. } => {
            let mut out = Vec::new();
            encode_error(&mut out, "ERR internal: watch accumulator hit materialize");
            out
        }
    }
}

/// Reduce keys collected from all shards into the final RESP reply.
fn finalize_keys(shape: KeyShape, acc: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    match shape {
        KeyShape::Keys => {
            encode_array_len(&mut out, acc.len() as i64);
            for k in &acc {
                encode_bulk(&mut out, k);
            }
        }
        KeyShape::Scan => {
            // [cursor, [keys]] — cursor "0" (non-incremental: one full pass).
            encode_array_len(&mut out, 2);
            encode_bulk(&mut out, b"0");
            encode_array_len(&mut out, acc.len() as i64);
            for k in &acc {
                encode_bulk(&mut out, k);
            }
        }
        KeyShape::Random => match acc.first() {
            Some(k) => encode_bulk(&mut out, k),
            None => encode_null_bulk(&mut out),
        },
    }
    out
}

/// Reduce gathered per-key payloads into the final RESP reply.
///
/// `proto` only affects the set-algebra arms (SINTER/SUNION/SDIFF):
/// RESP2 emits an `*N` array header, RESP3 a `~N` Set header. MGET
/// stays an `*N` array on both protos (per the RESP3 spec — order is
/// significant, can't be a Set).
fn finalize_gather(
    op: MultiOp,
    keys: Vec<Vec<u8>>,
    got: HashMap<Vec<u8>, Gathered>,
    proto: RespVersion,
) -> Vec<u8> {
    let mut out = Vec::new();
    match op {
        MultiOp::Mget => {
            encode_array_len(&mut out, keys.len() as i64);
            for k in &keys {
                match got.get(k) {
                    Some(Gathered::Str(Some(v))) => encode_bulk(&mut out, v),
                    _ => encode_null_bulk(&mut out), // missing / wrong-type → nil (MGET semantics)
                }
            }
        }
        _ => {
            let mut sets: Vec<Vec<Vec<u8>>> = Vec::with_capacity(keys.len());
            for k in &keys {
                match got.get(k) {
                    Some(Gathered::Members(m)) => sets.push(m.clone()),
                    Some(Gathered::WrongType) => {
                        encode_error(&mut out, WRONGTYPE);
                        return out;
                    }
                    _ => sets.push(Vec::new()), // missing key = empty set
                }
            }
            let result = match op {
                MultiOp::SInter => set_intersect(&sets),
                MultiOp::SUnion => set_union(&sets),
                MultiOp::SDiff => set_diff(&sets),
                // The outer match already routed Mget to its own arm
                // above; reaching this arm would mean the outer
                // dispatcher's wildcard caught Mget after a future
                // refactor. Replying empty is observably wrong but
                // doesn't crash the shard. Visible empty-array reply
                // makes the bug catchable; an `unreachable!()` would
                // crash-loop the whole reactor.
                MultiOp::Mget => Vec::new(),
            };
            match proto {
                RespVersion::V2 => encode_array_len(&mut out, result.len() as i64),
                RespVersion::V3 => encode_set_header(&mut out, result.len() as i64),
            }
            for m in &result {
                encode_bulk(&mut out, m);
            }
        }
    }
    out
}

/// Build a RESP pub/sub `message` push frame.
pub(crate) fn pubsub_message(channel: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_array_len(&mut out, 3);
    encode_bulk(&mut out, b"message");
    encode_bulk(&mut out, channel);
    encode_bulk(&mut out, msg);
    out
}

/// Build a RESP pub/sub `pmessage` push frame for `PSUBSCRIBE` delivery
/// (`*4\r\n$8\r\npmessage\r\n$<plen>\r\n<pat>\r\n$<clen>\r\n<chan>\r\n$<mlen>\r\n<payload>\r\n`).
pub(crate) fn pubsub_pmessage(pattern: &[u8], channel: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_array_len(&mut out, 4);
    encode_bulk(&mut out, b"pmessage");
    encode_bulk(&mut out, pattern);
    encode_bulk(&mut out, channel);
    encode_bulk(&mut out, msg);
    out
}

fn set_intersect(sets: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
    let Some((first, rest)) = sets.split_first() else {
        return Vec::new();
    };
    let mut acc: HashSet<&Vec<u8>> = first.iter().collect();
    for s in rest {
        let other: HashSet<&Vec<u8>> = s.iter().collect();
        acc.retain(|m| other.contains(*m));
    }
    acc.into_iter().cloned().collect()
}

fn set_union(sets: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
    let mut acc: HashSet<&Vec<u8>> = HashSet::new();
    for s in sets {
        for m in s {
            acc.insert(m);
        }
    }
    acc.into_iter().cloned().collect()
}

fn set_diff(sets: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
    let Some((first, rest)) = sets.split_first() else {
        return Vec::new();
    };
    let mut acc: HashSet<&Vec<u8>> = first.iter().collect();
    for s in rest {
        for m in s {
            acc.remove(m);
        }
    }
    acc.into_iter().cloned().collect()
}

/// Emit the contiguous prefix of completed slots in seq order.
pub(crate) fn drain_front(conn: &mut Conn) {
    while matches!(conn.pending.front(), Some(s) if s.done.is_some()) {
        let slot = conn.pending.pop_front().unwrap();
        if let Some(bytes) = slot.done {
            conn.output.extend_from_slice(&bytes);
        }
        conn.next_emit += 1;
    }
}

/// Shard index for `key` over `n` shards. Independent of the store's internal
/// hash so a cross-shard routing change doesn't require rehashing the store.
///
/// `n == 1` short-circuits to 0 (every key is local; common when running
/// `--threads 1` benchmarks). For `n > 1` use `kevy_hash::KevyHash` (FxFmix —
/// word-at-a-time, ~4× faster than the previous FNV-1a byte loop).
#[inline]
pub(crate) fn shard_of(key: &[u8], n: usize) -> usize {
    if n == 1 {
        return 0;
    }
    let h = key.kevy_hash();
    // Power-of-two n hits the cheap mask path; otherwise modulo.
    if n.is_power_of_two() {
        (h as usize) & (n - 1)
    } else {
        (h as usize) % n
    }
}
