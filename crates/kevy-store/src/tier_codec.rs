//! The vlog-payload codec for tiered (cold) values.
//!
//! Lives in kevy-store (NOT kevy-persist: the dependency arrow points
//! store → vlog, and persistence streams FROM the vlog). Two
//! shapes, one per v1-spillable class:
//!
//! - **bulk** (`COLD_TAG_STRING`): the raw value bytes, nothing else.
//! - **hash** (`COLD_TAG_HASH`): `[nfields u32-LE]` then per field
//!   `[flen u32-LE][fbytes][vlen u32-LE][vbytes]` — mirrors the
//!   snapshot payload shape for familiarity, but this is the vlog
//!   codec and versions independently.
//!
//! Hash field-TTLs (the `hfttl` side map) are deliberately NOT here:
//! they stay RAM-resident for cold hashes; promotion purges fields
//! that expired while cold via the existing lazy-purge path.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::value::{COLD_TAG_HASH, COLD_TAG_STRING, HashData, SmallBytes, Value};
use alloc::sync::Arc;

/// Encode a spillable value into its vlog payload. `None` for a value
/// outside the v1 spillable set (Str/Int/List/Set/ZSet/Stream/Cold) —
/// the demotion sampler skips those exactly like it skips Cold stubs.
pub(crate) fn encode(v: &Value) -> Option<(Vec<u8>, u8)> {
    match v {
        Value::ArcBulk(a) => Some((a.as_ref().to_vec(), COLD_TAG_STRING)),
        Value::Hash(h) => {
            let mut out = Vec::with_capacity(4 + h.len() * 16);
            out.extend_from_slice(&(h.len() as u32).to_le_bytes());
            for (f, val) in h.iter() {
                put_chunk(&mut out, f.as_slice());
                put_chunk(&mut out, val.as_slice());
            }
            Some((out, COLD_TAG_HASH))
        }
        Value::SmallHashInline(h) => {
            let mut out = Vec::with_capacity(4 + 22 + 8);
            out.extend_from_slice(&(h.len() as u32).to_le_bytes());
            for (f, val) in h.iter() {
                put_chunk(&mut out, f);
                put_chunk(&mut out, val);
            }
            Some((out, COLD_TAG_HASH))
        }
        _ => None,
    }
}

#[inline]
fn put_chunk(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Decode one vlog payload back into a live value. Errors only on a
/// malformed payload — which this process wrote this boot, so a decode
/// failure is a bug, not corruption to heal (the kevy-vlog doctrine).
pub(crate) fn decode(tag: u8, payload: Vec<u8>) -> Result<Value, &'static str> {
    match tag {
        COLD_TAG_STRING => {
            // Re-materialize through the SET encoding rules so a
            // demote/promote round trip lands on the same variant a
            // plain SET of these bytes would (ArcBulk for >64 B — the
            // only bytes that spill — keeping GET's writev path).
            Ok(crate::string_set::pick_value_for_set_owned(payload))
        }
        COLD_TAG_HASH => decode_hash(&payload),
        _ => Err("tier: unknown cold type tag"),
    }
}

fn decode_hash(p: &[u8]) -> Result<Value, &'static str> {
    let mut cur = 0usize;
    let n = read_u32(p, &mut cur)? as usize;
    let mut h = HashData::with_capacity(n.max(1));
    for _ in 0..n {
        let f = read_chunk(p, &mut cur)?;
        let v = read_chunk(p, &mut cur)?;
        h.insert(SmallBytes::from_slice(f), SmallBytes::from_slice(v));
    }
    if cur != p.len() {
        return Err("tier: hash payload has trailing bytes");
    }
    Ok(Value::Hash(Arc::new(h)))
}

fn read_u32(p: &[u8], cur: &mut usize) -> Result<u32, &'static str> {
    let end = cur.checked_add(4).ok_or("tier: offset overflow")?;
    let b: [u8; 4] = p
        .get(*cur..end)
        .ok_or("tier: truncated length")?
        .try_into()
        .expect("4-byte slice");
    *cur = end;
    Ok(u32::from_le_bytes(b))
}

fn read_chunk<'a>(p: &'a [u8], cur: &mut usize) -> Result<&'a [u8], &'static str> {
    let len = read_u32(p, cur)? as usize;
    let end = cur.checked_add(len).ok_or("tier: offset overflow")?;
    let out = p.get(*cur..end).ok_or("tier: truncated chunk")?;
    *cur = end;
    Ok(out)
}
