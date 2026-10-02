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
use alloc::sync::Arc;
use alloc::vec;
use core::num::NonZeroU64;

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
    pub fn setbit(&mut self, key: &[u8], offset: u64, value: u8) -> Result<u8, StoreError> {
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
        self.set_bytes_keep_ttl(key, owned);
        Ok(prev)
    }

    /// Store `owned` as `key`'s string, in the byte-array encoding (never
    /// int), keeping any TTL the key had.
    pub(crate) fn set_bytes_keep_ttl(&mut self, key: &[u8], owned: Vec<u8>) {
        let new_val = if owned.is_empty() {
            Value::Str(SmallBytes::from_slice(&[]))
        } else {
            Value::ArcBulk(Arc::new(owned.into_boxed_slice()))
        };
        // Entry stores `expire_at_ns: Option<NonZeroU64>` (absolute ns).
        let ttl_ns = self.live_entry(key).and_then(|e| e.expire_at_ns.map(NonZeroU64::get));
        self.insert_entry(SmallBytes::from_slice(key), Entry::new(new_val, ttl_ns));
    }

    /// `BITCOUNT key [start end]` — count set bits over a byte range
    /// (inclusive, negative from the end). `None` = the whole string.
    pub fn bitcount(&mut self, key: &[u8], range: Option<(i64, i64)>) -> Result<u64, StoreError> {
        self.bitcount_in(key, range.map(|(s, e)| (s, e, BitUnit::Byte)))
    }

    /// `BITCOUNT key [start end [BYTE | BIT]]`: the set bits in the range,
    /// counted in bytes or in bits as Redis counts them.
    ///
    /// ```
    /// use kevy_store::{BitUnit, SetCondition, Store};
    /// let mut s = Store::new();
    /// s.set(b"k", b"foobar".to_vec(), None, SetCondition::Always);
    /// assert_eq!(s.bitcount_in(b"k", Some((1, 1, BitUnit::Byte))).unwrap(), 6);
    /// assert_eq!(s.bitcount_in(b"k", Some((5, 30, BitUnit::Bit))).unwrap(), 17);
    /// ```
    pub fn bitcount_in(
        &mut self,
        key: &[u8],
        range: Option<(i64, i64, BitUnit)>,
    ) -> Result<u64, StoreError> {
        let Some(bytes) = self.get(key)? else { return Ok(0) };
        // two negatives the wrong way round count nothing, before either
        // is read against the length (BITPOS has no such rule)
        if range.is_some_and(|(s, e, _)| s < 0 && e < 0 && s > e) {
            return Ok(0);
        }
        let bits = bytes.len() as i64 * 8;
        let Some((from, to)) = bit_span(bits, range) else { return Ok(0) };
        let (first, last) = ((from / 8) as usize, (to / 8) as usize);
        Ok((first..=last)
            .map(|i| u64::from((bytes[i] & span_mask(i, from, to)).count_ones()))
            .sum())
    }

    /// `BITPOS key bit [start [end]]` over a byte range — see
    /// [`Self::bitpos_in`].
    pub fn bitpos(
        &mut self,
        key: &[u8],
        bit: u8,
        range: Option<(i64, i64)>,
    ) -> Result<Option<u64>, StoreError> {
        let (start, end) = range.map_or((None, None), |(s, e)| (Some(s), Some(e)));
        self.bitpos_in(key, bit, start, end, BitUnit::Byte)
    }

    /// `BITPOS key bit [start [end [BYTE | BIT]]]`: the first bit equal to
    /// `bit` (0 or 1) in the range, MSB first; `None` is Redis's `-1`.
    /// Looking for a 0 with no end given, a string of ones answers the bit
    /// just past it, since the string reads as zero-padded beyond its end.
    ///
    /// ```
    /// use kevy_store::{BitUnit, SetCondition, Store};
    /// let mut s = Store::new();
    /// s.set(b"k", b"\xff\xf0\x00".to_vec(), None, SetCondition::Always);
    /// assert_eq!(s.bitpos_in(b"k", 1, Some(7), Some(15), BitUnit::Bit).unwrap(), Some(7));
    /// assert_eq!(s.bitpos_in(b"k", 0, Some(0), Some(3), BitUnit::Bit).unwrap(), None);
    /// ```
    pub fn bitpos_in(
        &mut self,
        key: &[u8],
        bit: u8,
        start: Option<i64>,
        end: Option<i64>,
        unit: BitUnit,
    ) -> Result<Option<u64>, StoreError> {
        if bit > 1 {
            return Err(StoreError::OutOfRange);
        }
        let Some(bytes) = self.get(key)? else {
            return Ok((bit == 0).then_some(0));
        };
        let bits = bytes.len() as i64 * 8;
        let range = (start.unwrap_or(0), end.unwrap_or(-1), unit);
        let Some((from, to)) = bit_span(bits, Some(range)) else { return Ok(None) };
        for i in (from / 8) as usize..=(to / 8) as usize {
            let b = if bit == 1 { bytes[i] } else { !bytes[i] } & span_mask(i, from, to);
            if b != 0 {
                return Ok(Some(i as u64 * 8 + u64::from(b.leading_zeros())));
            }
        }
        Ok((bit == 0 && end.is_none()).then_some(to as u64 + 1))
    }

    /// `GETRANGE key start end` — substring with Redis-style
    /// negative indexing; `[start, end]` inclusive. Returns empty
    /// `Vec` when key absent or range out of bounds.
    pub fn getrange(&mut self, key: &[u8], start: i64, end: i64) -> Result<Vec<u8>, StoreError> {
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
    pub fn setrange(&mut self, key: &[u8], offset: u64, value: &[u8]) -> Result<usize, StoreError> {
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
        let ttl_ns = self.live_entry(key).and_then(|e| e.expire_at_ns.map(NonZeroU64::get));
        self.insert_entry(SmallBytes::from_slice(key), Entry::new(new_val, ttl_ns));
        Ok(new_len)
    }
}

/// The unit of a `BITCOUNT` / `BITPOS` range.
///
/// ```
/// use kevy_store::BitUnit;
/// assert_ne!(BitUnit::Byte, BitUnit::Bit);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BitUnit {
    /// Offsets count bytes, as Redis's default.
    Byte,
    /// Offsets count bits, MSB first.
    Bit,
}

/// The inclusive bit range `[start, end]` of a string `bits` long, read
/// as Redis reads it: negative from the end, floored at 0, the end capped
/// at the last bit. `None` (no range) is the whole string; an empty
/// result is `None`.
fn bit_span(bits: i64, range: Option<(i64, i64, BitUnit)>) -> Option<(i64, i64)> {
    let Some((start, end, unit)) = range else {
        return (bits > 0).then_some((0, bits - 1));
    };
    let len = if unit == BitUnit::Bit { bits } else { bits / 8 };
    let from_end = |x: i64| if x < 0 { len.saturating_add(x).max(0) } else { x };
    let (s, e) = (from_end(start), from_end(end).min(len - 1));
    if s > e {
        return None;
    }
    Some(if unit == BitUnit::Bit { (s, e) } else { (s * 8, e * 8 + 7) })
}

/// The bits of byte `i` that fall inside `[from, to]`.
fn span_mask(i: usize, from: i64, to: i64) -> u8 {
    let lo = (from - i as i64 * 8).clamp(0, 8) as u32;
    let hi = (i as i64 * 8 + 7 - to).clamp(0, 8) as u32;
    (0xffu16 >> lo) as u8 & (0xffu16 << hi) as u8
}

// ── BITOP: the operator, and the byte arithmetic ───────────────────
//
// Both live here rather than in a facade because neither knows what a
// key is. `kevy-embedded` computed them for its own BITOP and
// `kevy-rt` could not reach that code at all — sibling crates — so
// wiring BITOP to the server wire would have meant a second copy of
// the padding rules, the 0xff tail of NOT among them. Two
// implementations of one operator are how two surfaces drift.

/// Operator for the BITOP family.
///
/// ```
/// use kevy_store::BitOp;
/// // NOT takes exactly one source; the callers enforce that, and this
/// // is what it computes.
/// assert_eq!(BitOp::Not.combine(&[vec![0b1010_1010]], 1), vec![0b0101_0101]);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BitOp {
    /// Bitwise AND across source keys.
    ///
    /// ```
    /// use kevy_store::BitOp;
    /// assert_eq!(BitOp::And.combine(&[vec![0b1100], vec![0b1010]], 1), vec![0b1000]);
    /// ```
    And,
    /// Bitwise OR across source keys.
    ///
    /// ```
    /// use kevy_store::BitOp;
    /// assert_eq!(BitOp::Or.combine(&[vec![0b1100], vec![0b1010]], 1), vec![0b1110]);
    /// ```
    Or,
    /// Bitwise XOR across source keys.
    ///
    /// ```
    /// use kevy_store::BitOp;
    /// assert_eq!(BitOp::Xor.combine(&[vec![0b1100], vec![0b1010]], 1), vec![0b0110]);
    /// ```
    Xor,
    /// Bitwise NOT — exactly one source key.
    ///
    /// ```
    /// use kevy_store::BitOp;
    /// assert_eq!(BitOp::Not.combine(&[vec![0x0f]], 1), vec![0xf0]);
    /// ```
    Not,
    /// The bits of the first source set in none of the others:
    /// `X ∧ ¬(Y1 ∨ Y2 …)`.
    ///
    /// ```
    /// use kevy_store::BitOp;
    /// assert_eq!(BitOp::Diff.combine(&[vec![0xf0], vec![0x3c]], 1), vec![0xc0]);
    /// ```
    Diff,
    /// The bits set in some other source and not in the first:
    /// `¬X ∧ (Y1 ∨ Y2 …)`.
    ///
    /// ```
    /// use kevy_store::BitOp;
    /// assert_eq!(BitOp::Diff1.combine(&[vec![0xf0], vec![0x3c]], 1), vec![0x0c]);
    /// ```
    Diff1,
    /// The bits of the first source set in some other: `X ∧ (Y1 ∨ Y2 …)`.
    ///
    /// ```
    /// use kevy_store::BitOp;
    /// assert_eq!(BitOp::AndOr.combine(&[vec![0xf0], vec![0x3c]], 1), vec![0x30]);
    /// ```
    AndOr,
    /// The bits set in exactly one source.
    ///
    /// ```
    /// use kevy_store::BitOp;
    /// let srcs = [vec![0xf0], vec![0x3c], vec![0x0f]];
    /// assert_eq!(BitOp::One.combine(&srcs, 1), vec![0xc3]);
    /// ```
    One,
}

impl BitOp {
    /// The least number of source keys the operator takes: NOT takes one
    /// and only one, DIFF / DIFF1 / ANDOR at least two, the rest one.
    ///
    /// ```
    /// assert_eq!(kevy_store::BitOp::Diff.min_sources(), 2);
    /// ```
    #[must_use]
    pub fn min_sources(self) -> usize {
        match self {
            BitOp::Diff | BitOp::Diff1 | BitOp::AndOr => 2,
            _ => 1,
        }
    }

    /// Combine the source strings under this operator into the `max_len`-byte
    /// destination value (shorter sources zero-padded).
    ///
    /// Two rules are easy to get wrong and both are here. A source shorter
    /// than the result reads as zero past its end — so an AND with a short
    /// source clears the tail, and an OR leaves it alone. And NOT does not
    /// stop at its source: Redis inverts the implicit zeros too, so every
    /// byte past the source is `0xff`.
    ///
    /// ```
    /// use kevy_store::BitOp;
    ///
    /// let long = b"\xff\xff".to_vec();
    /// let short = b"\x0f".to_vec();
    /// // AND: the second byte meets an implicit zero.
    /// assert_eq!(BitOp::And.combine(&[long.clone(), short.clone()], 2), vec![0x0f, 0x00]);
    /// // OR: the implicit zero changes nothing.
    /// assert_eq!(BitOp::Or.combine(&[long.clone(), short], 2), vec![0xff, 0xff]);
    /// // NOT over a two-byte result from a one-byte source: the tail is 0xff.
    /// assert_eq!(BitOp::Not.combine(&[vec![0x00]], 2), vec![0xff, 0xff]);
    /// ```
    #[must_use]
    pub fn combine(self, srcs_bytes: &[Vec<u8>], max_len: usize) -> Vec<u8> {
        let mut out = vec![0u8; max_len];
        match self {
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
            BitOp::Diff | BitOp::Diff1 | BitOp::AndOr => {
                self.first_against_rest(srcs_bytes, &mut out)
            }
            BitOp::One => exactly_one(srcs_bytes, &mut out),
            // AND, OR, XOR. NOT returned above, so the catch-alls below are
            // XOR — written as `_` rather than `Not => unreachable!()`,
            // which was four arms that can never run and four regions that
            // can never be covered.
            _ => {
                let init = if self == BitOp::And { 0xff } else { 0x00 };
                for byte in out.iter_mut() {
                    *byte = init;
                }
                for s in srcs_bytes {
                    for (i, b) in out.iter_mut().enumerate() {
                        let sb = s.get(i).copied().unwrap_or(0);
                        *b = match self {
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

    /// DIFF, DIFF1 and ANDOR: the first source against the OR of the rest.
    fn first_against_rest(self, srcs_bytes: &[Vec<u8>], out: &mut [u8]) {
        let none = Vec::new();
        let (first, rest) = srcs_bytes.split_first().unwrap_or((&none, &[]));
        for (i, b) in out.iter_mut().enumerate() {
            let x = first.get(i).copied().unwrap_or(0);
            let any = rest.iter().fold(0, |acc, s| acc | s.get(i).copied().unwrap_or(0));
            *b = match self {
                BitOp::Diff => x & !any,
                BitOp::Diff1 => !x & any,
                _ => x & any,
            };
        }
    }
}

/// ONE: the bits set in exactly one source. `out` holds the bits seen once
/// so far, `more` those seen twice or more.
fn exactly_one(srcs_bytes: &[Vec<u8>], out: &mut [u8]) {
    let mut more = vec![0u8; out.len()];
    for s in srcs_bytes {
        for (i, (once, more)) in out.iter_mut().zip(&mut more).enumerate() {
            let sb = s.get(i).copied().unwrap_or(0);
            *more |= *once & sb;
            *once = (*once ^ sb) & !*more;
        }
    }
}
