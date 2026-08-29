//! Bitmap ops on string-typed values — `SETBIT` / `GETBIT` /
//! `BITCOUNT`. Redis treats strings as byte arrays addressed at the
//! bit level; this module exposes those reads / writes against the
//! existing string value encodings (`Value::Str` / `Value::ArcBulk` /
//! `Value::Int`).
//!
//! Split out from `string.rs` to keep that file under the 500-LOC
//! house rule.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use crate::util::range_bounds;
use alloc::borrow::Cow;
use alloc::vec;
use core::num::NonZeroU64;
use alloc::sync::Arc;

use crate::value::{SmallBytes, Value};
use crate::{Entry, Store, StoreError};

impl Store {
    /// `GETBIT key offset` — read the bit at `offset` (MSB-first
    /// within each byte, matching Redis). Returns `0` for missing
    /// key or offset past the end. Errors on wrong type.
    pub fn getbit(&mut self, key: &[u8], offset: u64) -> Result<u8, StoreError> {
        let bytes = match self.get(key)? {
            Some(cow) => cow,
            None => return Ok(0),
        };
        let byte_idx = (offset / 8) as usize;
        let bit_idx = 7 - (offset % 8) as u8;
        if byte_idx >= bytes.len() {
            return Ok(0);
        }
        Ok((bytes[byte_idx] >> bit_idx) & 1)
    }

    /// `SETBIT key offset value` — set the bit at `offset` to `value`
    /// (0 or 1). Extends the underlying string with zero-padding if
    /// `offset / 8 >= current_len`. Returns the PREVIOUS bit value.
    /// Errors on wrong type or `value > 1`.
    pub fn setbit(
        &mut self,
        key: &[u8],
        offset: u64,
        value: u8,
    ) -> Result<u8, StoreError> {
        if value > 1 {
            return Err(StoreError::OutOfRange);
        }
        let byte_idx = (offset / 8) as usize;
        let bit_idx = 7 - (offset % 8) as u8;

        // Read current bytes (Cow); compute previous bit; extend +
        // write back. We collect into a fresh Vec each time — bitmaps
        // tend to be hot-write so SmallBytes shrink-fit is moot.
        let mut owned: Vec<u8> = match self.get(key)? {
            Some(Cow::Borrowed(b)) => b.to_vec(),
            Some(Cow::Owned(v)) => v,
            None => Vec::new(),
        };
        if byte_idx >= owned.len() {
            owned.resize(byte_idx + 1, 0);
        }
        let prev = (owned[byte_idx] >> bit_idx) & 1;
        if value == 1 {
            owned[byte_idx] |= 1 << bit_idx;
        } else {
            owned[byte_idx] &= !(1u8 << bit_idx);
        }
        // Store back. Always use the byte-array encoding (never int).
        let new_val = if owned.is_empty() {
            Value::Str(SmallBytes::from_slice(&[]))
        } else {
            Value::ArcBulk(Arc::new(owned.into_boxed_slice()))
        };
        // Take any existing TTL, re-attach to the new entry. Entry
        // stores `expire_at_ns: Option<NonZeroU64>` (absolute ns).
        let ttl_ns = self
            .live_entry(key)
            .and_then(|e| e.expire_at_ns.map(NonZeroU64::get));
        self.insert_entry(
            SmallBytes::from_slice(key),
            Entry::new(new_val, ttl_ns),
        );
        Ok(prev)
    }

    /// `BITCOUNT key [start end [BYTE|BIT]]` — count set bits.
    /// `start`/`end` are byte offsets (inclusive, negative-from-tail
    /// like Redis). `None` for both = whole string.
    pub fn bitcount(
        &mut self,
        key: &[u8],
        range: Option<(i64, i64)>,
    ) -> Result<u64, StoreError> {
        let bytes = match self.get(key)? {
            Some(cow) => cow,
            None => return Ok(0),
        };
        if bytes.is_empty() {
            return Ok(0);
        }
        let len = bytes.len() as i64;
        let (s, e) = match range {
            None => (0, (len - 1) as usize),
            Some((start, end)) => {
                let norm = |x: i64| -> i64 {
                    if x < 0 { (len + x).max(0) } else { x.min(len - 1) }
                };
                let s = norm(start);
                let e = norm(end);
                if s > e {
                    return Ok(0);
                }
                (s as usize, e as usize)
            }
        };
        Ok(bytes[s..=e]
            .iter()
            .map(|b| u64::from(b.count_ones()))
            .sum())
    }

    /// `BITPOS key bit [start [end]]` — return the position (bit
    /// index, MSB-first) of the first bit equal to `bit` (0 or 1)
    /// in the byte range `[start, end]` (inclusive, Redis-style
    /// negative indexing). Returns `None` (Redis `-1`) when not
    /// found. Errors with `OutOfRange` if `bit` > 1.
    pub fn bitpos(
        &mut self,
        key: &[u8],
        bit: u8,
        range: Option<(i64, i64)>,
    ) -> Result<Option<u64>, StoreError> {
        if bit > 1 {
            return Err(StoreError::OutOfRange);
        }
        let bytes = match self.get(key)? {
            Some(cow) => cow,
            None => return Ok(if bit == 0 { Some(0) } else { None }),
        };
        if bytes.is_empty() {
            return Ok(if bit == 0 { Some(0) } else { None });
        }
        let len = bytes.len() as i64;
        let (s, e) = match range {
            None => (0usize, (len - 1) as usize),
            Some((start, end)) => {
                let norm = |x: i64| -> i64 {
                    if x < 0 { (len + x).max(0) } else { x.min(len - 1) }
                };
                let s = norm(start);
                let e = norm(end);
                if s > e {
                    return Ok(None);
                }
                (s as usize, e as usize)
            }
        };
        for (i, &b) in bytes[s..=e].iter().enumerate() {
            let target_mask = if bit == 1 { b } else { !b };
            if target_mask != 0 {
                let bit_in_byte = target_mask.leading_zeros() as u64;
                let byte_idx = (s + i) as u64;
                return Ok(Some(byte_idx * 8 + bit_in_byte));
            }
        }
        Ok(None)
    }

    /// `GETRANGE key start end` — substring with Redis-style
    /// negative indexing; `[start, end]` inclusive. Returns empty
    /// `Vec` when key absent or range out of bounds.
    pub fn getrange(
        &mut self,
        key: &[u8],
        start: i64,
        end: i64,
    ) -> Result<Vec<u8>, StoreError> {
        let bytes = match self.get(key)? {
            Some(cow) => cow,
            None => return Ok(Vec::new()),
        };
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        // `range_bounds`, not a clamp of its own. This function had
        // one, and it capped START at len-1 as well as END — so
        // `GETRANGE k 99 200` on a 24-byte value answered the last byte
        // where Redis answers nothing. Redis floors a negative start at
        // zero and caps only the end; a start past the last index makes
        // the range empty. The three-way differential against a real
        // valkey is what found it, after the wire-vs-facade one had
        // agreed — both surfaces shared the mistake, so comparing them
        // proved nothing about Redis.
        Ok(match range_bounds(start, end, bytes.len()) {
            None => Vec::new(),
            Some((s, e)) => bytes[s..=e].to_vec(),
        })
    }

    /// `SETRANGE key offset value` — overwrite bytes at `offset`
    /// with `value`. Extends the string with zero padding if
    /// `offset > len`. Returns the new total length. Preserves
    /// any existing TTL.
    pub fn setrange(
        &mut self,
        key: &[u8],
        offset: u64,
        value: &[u8],
    ) -> Result<usize, StoreError> {
        let offset = offset as usize;
        let mut owned: Vec<u8> = match self.get(key)? {
            Some(Cow::Borrowed(b)) => b.to_vec(),
            Some(Cow::Owned(v)) => v,
            None => Vec::new(),
        };
        let needed = offset + value.len();
        if needed > owned.len() {
            owned.resize(needed, 0);
        }
        owned[offset..offset + value.len()].copy_from_slice(value);
        let new_len = owned.len();
        let new_val = if owned.is_empty() {
            Value::Str(SmallBytes::from_slice(&[]))
        } else {
            Value::ArcBulk(Arc::new(owned.into_boxed_slice()))
        };
        let ttl_ns = self
            .live_entry(key)
            .and_then(|e| e.expire_at_ns.map(NonZeroU64::get));
        self.insert_entry(
            SmallBytes::from_slice(key),
            Entry::new(new_val, ttl_ns),
        );
        Ok(new_len)
    }
}

// ── BITOP: the operator, and the byte arithmetic ───────────────────
//
// Both live here rather than in a facade because neither knows what a
// key is. `kevy-embedded` computed them for its own BITOP and
// `kevy-rt` could not reach that code at all — sibling crates — so
// wiring BITOP to the server wire would have meant a second copy of
// the padding rules, the 0xff tail of NOT among them. Two
// implementations of one operator are how two surfaces drift.

/// Combine the source strings under `op` into the `max_len`-byte
/// destination value (shorter sources zero-padded).
///
/// Two rules are easy to get wrong and both are here. A source shorter
/// than the result reads as zero past its end — so an AND with a short
/// source clears the tail, and an OR leaves it alone. And NOT does not
/// stop at its source: Redis inverts the implicit zeros too, so every
/// byte past the source is `0xff`.
///
/// ```
/// use kevy_store::{BitOp, bitop_combine};
///
/// let long = b"\xff\xff".to_vec();
/// let short = b"\x0f".to_vec();
/// // AND: the second byte meets an implicit zero.
/// assert_eq!(bitop_combine(BitOp::And, &[long.clone(), short.clone()], 2), vec![0x0f, 0x00]);
/// // OR: the implicit zero changes nothing.
/// assert_eq!(bitop_combine(BitOp::Or, &[long.clone(), short], 2), vec![0xff, 0xff]);
/// // NOT over a two-byte result from a one-byte source: the tail is 0xff.
/// assert_eq!(bitop_combine(BitOp::Not, &[vec![0x00]], 2), vec![0xff, 0xff]);
/// ```
pub fn bitop_combine(op: BitOp, srcs_bytes: &[Vec<u8>], max_len: usize) -> Vec<u8> {
    let mut out = vec![0u8; max_len];
    match op {
        BitOp::Not => {
            let s = &srcs_bytes[0];
            for (i, b) in s.iter().enumerate() {
                out[i] = !b;
            }
            // bytes past s.len() stay 0 — Redis sets them to 0xff
            // (NOT of implicit zero). Match Redis:
            for byte in out.iter_mut().skip(s.len()) {
                *byte = 0xff;
            }
        }
        // AND, OR, XOR. NOT returned above, so the catch-alls below are
        // XOR — written as `_` rather than `Not => unreachable!()`,
        // which was four arms that can never run and four regions that
        // can never be covered.
        _ => {
            let init = if op == BitOp::And { 0xff } else { 0x00 };
            for byte in out.iter_mut() {
                *byte = init;
            }
            for s in srcs_bytes {
                for (i, b) in out.iter_mut().enumerate() {
                    let sb = s.get(i).copied().unwrap_or(0);
                    *b = match op {
                        BitOp::And => *b & sb,
                        BitOp::Or => *b | sb,
                        _ => *b ^ sb,
                    };
                }
            }
        }
    }
    out
}

/// Operator for the BITOP family.
///
/// ```
/// use kevy_store::{BitOp, bitop_combine};
/// // NOT takes exactly one source; the callers enforce that, and this
/// // is what it computes.
/// assert_eq!(bitop_combine(BitOp::Not, &[vec![0b1010_1010]], 1), vec![0b0101_0101]);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitOp {
    /// Bitwise AND across source keys.
    And,
    /// Bitwise OR across source keys.
    Or,
    /// Bitwise XOR across source keys.
    Xor,
    /// Bitwise NOT — exactly one source key.
    Not,
}
