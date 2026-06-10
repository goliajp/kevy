//! Reply reduction and small pure helpers.
//!
//! [`materialize`] turns a completed [`Agg`] accumulator into final RESP bytes;
//! the rest are the stateless pieces used across the runtime — set algebra,
//! pub/sub framing, the seq-ring drain, and the shard hash.

use crate::conn::Conn;
use crate::message::{Agg, Gathered, KeyShape, MultiOp, SmallReply};
use kevy_hash::KevyHash;
use kevy_resp::{
    RespVersion, encode_array_len, encode_bulk, encode_error, encode_null_bulk,
    encode_push_header, encode_set_header,
};
use std::collections::{HashMap, HashSet};

const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";

/// Turn a completed accumulator into its final RESP reply bytes. The
/// per-conn `proto` flips one or two reply shapes (notably the set-
/// algebra arms of `finalize_gather` — SINTER/SUNION/SDIFF go from
/// `*N` Array to `~N` Set under RESP3). Every other arm is the same
/// bytes on both protos.
pub(crate) fn materialize(agg: Agg, proto: RespVersion) -> SmallReply {
    match agg {
        Agg::First(Some(b)) => b,
        Agg::First(None) => {
            let mut out = Vec::new();
            encode_error(&mut out, "ERR internal error");
            SmallReply::from_vec(out)
        }
        // `:N` is ≤ 22 bytes — inline, no alloc.
        Agg::SumInt(n) => {
            let mut out = [0u8; 30];
            let mut cur = std::io::Cursor::new(&mut out[..]);
            use std::io::Write as _;
            let _ = write!(cur, ":{n}\r\n");
            let len = cur.position() as u8;
            SmallReply::Inline { len, buf: out }
        }
        Agg::AllOk => SmallReply::from_slice(b"+OK\r\n"),
        Agg::Gather { op, keys, got } => {
            SmallReply::from_vec(finalize_gather(op, keys, got, proto))
        }
        Agg::XReadGather { slots } => SmallReply::from_vec(finalize_xread_gather(slots)),
        Agg::Keys { shape, acc } => SmallReply::from_vec(finalize_keys(shape, acc)),
        Agg::SlowlogGet { count, entries } => {
            SmallReply::from_vec(crate::exec_slowlog::encode_slowlog_get(count, entries))
        }
        // WatchCollect / ExecPrep / RenameOrchestrator carry conn-
        // state mutations that pure materialise() can't express;
        // `Shard::fold` routes them to `finalize_watch_agg` (Watch /
        // Exec) or `finalize_rename_agg` (Rename) instead, so they
        // never reach here. Defensive: emit an error rather than
        // silently dropping the slot — a misroute would otherwise
        // hang the connection.
        Agg::WatchCollect { .. } | Agg::ExecPrep { .. } | Agg::RenameOrchestrator { .. } => {
            let mut out = Vec::new();
            encode_error(&mut out, "ERR internal: orchestrator agg hit materialize");
            SmallReply::from_vec(out)
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

/// Reassemble a cross-shard non-blocking `XREAD` reply from per-stream slots
/// in request order. Empty streams (`None`) are skipped; if every stream was
/// empty the reply is `*-1` (matching single-shard non-blocking XREAD). If
/// any stream returned an error frame (leading `-`) it's surfaced as the
/// whole reply — Redis fails the command on the first wrong-type / bad-id
/// stream. XREAD has no RESP3 shape, so this is proto-independent.
fn finalize_xread_gather(slots: Vec<Option<Vec<u8>>>) -> Vec<u8> {
    for slot in slots.iter().flatten() {
        if slot.first() == Some(&b'-') {
            return slot.clone();
        }
    }
    let elements: Vec<&Vec<u8>> = slots.iter().flatten().collect();
    if elements.is_empty() {
        return b"*-1\r\n".to_vec();
    }
    let mut out = Vec::new();
    encode_array_len(&mut out, elements.len() as i64);
    for e in elements {
        out.extend_from_slice(e);
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

/// Build a RESP pub/sub `message` delivery frame. V2 emits a 3-element
/// array (`*3\r\n…`); V3 emits a Push frame (`>3\r\n…`) — same body
/// bytes, prefix flips so the V3 client demuxes pub/sub from regular
/// replies. Per-conn proto, since one channel can have V2 + V3
/// subscribers mixed.
pub(crate) fn pubsub_message(channel: &[u8], msg: &[u8], proto: RespVersion) -> Vec<u8> {
    let mut out = Vec::new();
    match proto {
        RespVersion::V2 => encode_array_len(&mut out, 3),
        RespVersion::V3 => encode_push_header(&mut out, 3),
    }
    encode_bulk(&mut out, b"message");
    encode_bulk(&mut out, channel);
    encode_bulk(&mut out, msg);
    out
}

/// Build a RESP pub/sub `pmessage` delivery frame (PSUBSCRIBE matches).
/// V2 `*4\r\n…` Array vs V3 `>4\r\n…` Push frame — same body, prefix
/// flips.
pub(crate) fn pubsub_pmessage(
    pattern: &[u8],
    channel: &[u8],
    msg: &[u8],
    proto: RespVersion,
) -> Vec<u8> {
    let mut out = Vec::new();
    match proto {
        RespVersion::V2 => encode_array_len(&mut out, 4),
        RespVersion::V3 => encode_push_header(&mut out, 4),
    }
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
            conn.output.extend_from_slice(bytes.as_slice());
        }
        conn.next_emit += 1;
    }
}

/// Shard index for `key` over `n` shards. Independent of the store's internal
/// hash so a cross-shard routing change doesn't require rehashing the store.
///
/// `n == 1` short-circuits to 0 (every key is local; common when running
/// `--threads 1` benchmarks). Two routing schemes (`slots`):
///
/// - `false` (default): `kevy_hash::KevyHash` (FxFmix — word-at-a-time,
///   ~4× faster than the previous FNV-1a byte loop).
/// - `true` (cluster mode): Redis-cluster slots — `key_hash_slot` (CRC16 of
///   the `{hashtag}` & 16383) then [`slot_to_shard`], so external cluster
///   clients can compute key placement themselves.
///
/// The scheme is a startup-time property of the data dir (`shards.meta`),
/// never flipped at runtime.
#[inline]
pub(crate) fn shard_of(key: &[u8], n: usize, slots: bool) -> usize {
    if n == 1 {
        return 0;
    }
    if slots {
        return slot_to_shard(kevy_hash::key_hash_slot(key), n);
    }
    let h = key.kevy_hash();
    // Power-of-two n hits the cheap mask path; otherwise modulo.
    if n.is_power_of_two() {
        (h as usize) & (n - 1)
    } else {
        (h as usize) % n
    }
}

/// Owner shard of a cluster `slot` under the contiguous even split: shard `i`
/// owns `[ceil(i·16384/n), ceil((i+1)·16384/n))`, for which `(slot·n) >> 14`
/// is the exact inverse (16384 = 2¹⁴ — multiply + shift, no division).
#[inline]
pub(crate) fn slot_to_shard(slot: u16, n: usize) -> usize {
    (slot as usize * n) >> 14
}
