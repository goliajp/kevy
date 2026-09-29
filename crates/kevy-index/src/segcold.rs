//! Cold-segment support for scalar indexes: the order-preserving
//! `(value, row_key)` byte-key codec an evicted tree entry is stored
//! under, and the membership bloom that tells the write path a row MAY
//! have a cold entry (so it earns a tombstone instead of a ghost).
//!
//! The key is `frame(order_bytes(value)) ‖ frame(row_key)` — the
//! composite codec's escape-and-terminate framing (0x00 → 0x00 0xFF,
//! terminator 0x00 0x00; the terminator sorts below every escaped
//! continuation byte, so memcmp order equals tuple order), without the
//! composite column-length cap: a scalar index puts no bound on str
//! values or row keys, so neither does its cold key.

use crate::catalog::ValType;
use crate::value::IndexValue;

pub(crate) fn value_order_bytes(v: &IndexValue) -> Vec<u8> {
    match v {
        IndexValue::Str(s) => s.clone(),
        IndexValue::I64(i) => ((*i as u64) ^ (1 << 63)).to_be_bytes().to_vec(),
        IndexValue::F64(f) => {
            let b = f.to_bits();
            let m = if b >> 63 == 1 { !b } else { b | (1 << 63) };
            m.to_be_bytes().to_vec()
        }
    }
}

/// `(value, row_key)` → the strictly-orderable segment key.
///
/// ```
/// use kevy_index::{IndexValue, seg_key};
/// // memcmp order of the keys is (value, row_key) tuple order
/// let a = seg_key(&IndexValue::I64(-5), b"zz");
/// let b = seg_key(&IndexValue::I64(3), b"aa");
/// let c = seg_key(&IndexValue::I64(3), b"ab");
/// assert!(a < b && b < c);
/// ```
pub fn seg_key(v: &IndexValue, row_key: &[u8]) -> Vec<u8> {
    let vb = v.order_bytes();
    let mut out = Vec::with_capacity(vb.len() + row_key.len() + 4);
    frame_into(&mut out, &vb);
    frame_into(&mut out, row_key);
    out
}

/// Decode a segment key back to `(value, row_key)`. `ty` is the
/// index's declared type (the key does not carry it). `None` on any
/// malformed frame — a corrupt key is a refusal upstream, never a
/// guessed entry.
///
/// ```
/// use kevy_index::{IndexValue, ValType, decode_seg_key, seg_key};
/// let key = seg_key(&IndexValue::F64(-2.5), b"row\0:1");
/// let (v, row) = decode_seg_key(ValType::F64, &key).expect("well-formed");
/// assert_eq!((v, row), (IndexValue::F64(-2.5), b"row\0:1".to_vec()));
/// assert_eq!(decode_seg_key(ValType::F64, &key[..key.len() - 1]), None);
/// ```
pub fn decode_seg_key(ty: ValType, key: &[u8]) -> Option<(IndexValue, Vec<u8>)> {
    let (vb, rest) = unframe(key)?;
    let (row, tail) = unframe(rest)?;
    if !tail.is_empty() {
        return None;
    }
    let value = match ty {
        ValType::Str => IndexValue::Str(vb),
        ValType::I64 => {
            let raw = u64::from_be_bytes(vb.try_into().ok()?);
            IndexValue::I64((raw ^ (1 << 63)) as i64)
        }
        ValType::F64 => {
            let m = u64::from_be_bytes(vb.try_into().ok()?);
            let b = if m >> 63 == 1 { m & !(1 << 63) } else { !m };
            IndexValue::F64(f64::from_bits(b))
        }
        _ => return None,
    };
    Some((value, row))
}

/// Inclusive byte bounds covering exactly the cold keys whose value
/// lies in `[min, max]`. The upper bound is the max value's frame with
/// its final terminator byte flipped to 0x01: every `frame(max) ‖
/// frame(row)` key sorts below it (the shared prefix ends 0x00 0x00 <
/// 0x00 0x01), and no larger value's frame can reach it (in the
/// escaped alphabet 0x00 is always followed by 0xFF, so the flipped
/// terminator is nobody's prefix).
///
/// ```
/// use kevy_index::{IndexValue, seg_bounds, seg_key};
/// let (lo, hi) = seg_bounds(&IndexValue::I64(10), &IndexValue::I64(20));
/// let within = |v: i64| {
///     let k = seg_key(&IndexValue::I64(v), b"row");
///     lo <= k && k <= hi
/// };
/// assert!(within(10) && within(15) && within(20));
/// assert!(!within(9) && !within(21));
/// ```
pub fn seg_bounds(min: &IndexValue, max: &IndexValue) -> (Vec<u8>, Vec<u8>) {
    let mut lo = Vec::new();
    frame_into(&mut lo, &min.order_bytes());
    let mut hi = Vec::new();
    frame_into(&mut hi, &max.order_bytes());
    *hi.last_mut().expect("frame is never empty") = 0x01;
    (lo, hi)
}

/// Which shape of tree a window boundary lives in: a plain i64
/// window-column index, or a composite index the window column LEADS
/// (ascending — its first component is the column's `order_key` 8B,
/// so the tree prefix below a boundary is exactly the out-of-window
/// batch, the property the slide rides).
///
/// ```
/// use kevy_index::{IndexValue, WindowShape, window_bound};
/// // the same boundary, expressed in each tree's value space
/// assert_eq!(window_bound(7, WindowShape::PlainI64), IndexValue::I64(7));
/// assert!(matches!(window_bound(7, WindowShape::CompositeLed), IndexValue::Str(b) if b.len() == 8));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WindowShape {
    /// `INDEX <wcol> range` — the tree's values ARE the window column.
    ///
    /// ```
    /// use kevy_index::{IndexValue, WindowShape};
    /// assert_eq!(IndexValue::I64(1_700).window_value(WindowShape::PlainI64), Some(1_700));
    /// ```
    PlainI64,
    /// `ORDERPATH` led by the window column ascending — the values
    /// are composite byte strings whose first 8 bytes encode it.
    ///
    /// ```
    /// use kevy_index::{IndexValue, WindowShape, order_key, ValType};
    /// let mut composite = order_key(ValType::I64, b"1700").expect("an i64");
    /// composite.extend_from_slice(b"trailing columns");
    /// let v = IndexValue::Str(composite);
    /// assert_eq!(v.window_value(WindowShape::CompositeLed), Some(1_700));
    /// ```
    CompositeLed,
}

/// What an audit needs to tell "slid out on purpose" from "lost".
///
/// A row whose window value sits below `boundary` is absent from the
/// hot tree by design — but only if it actually reached a cold
/// segment. Position alone cannot distinguish the two, and a row lost
/// between the two structures sits below the boundary exactly like a
/// legitimate one. So the audit carries the cold side's own count and
/// checks the identity instead:
///
///   missing = (rows below the boundary that should be indexed)
///             - (live cold entries below it)
///
/// One `u64` out of the segment scope, no per-row cold lookup, and the
/// answer is evidence rather than assumption.
///
/// ```
/// use kevy_index::{WindowAudit, WindowShape};
/// let a = WindowAudit::new(1_000, WindowShape::PlainI64, 42);
/// assert_eq!((a.boundary, a.cold_live), (1_000, 42));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct WindowAudit {
    /// Bucket-aligned eviction boundary: entries below it are cold.
    ///
    /// ```
    /// use kevy_index::{IndexValue, WindowAudit, WindowShape};
    /// let a = WindowAudit::new(1_000, WindowShape::PlainI64, 0);
    /// // a row whose window value sits below the boundary belongs to the cold side
    /// let v = IndexValue::I64(999).window_value(a.shape).expect("plain i64");
    /// assert!(v < a.boundary);
    /// ```
    pub boundary: i64,
    /// Which tree shape the boundary lives in.
    ///
    /// ```
    /// use kevy_index::{WindowAudit, WindowShape, window_bound};
    /// let a = WindowAudit::new(5, WindowShape::CompositeLed, 0);
    /// // the shape picks the value space the boundary is compared in
    /// assert_eq!(window_bound(a.boundary, a.shape), window_bound(5, WindowShape::CompositeLed));
    /// ```
    pub shape: WindowShape,
    /// Live (non-tombstoned) cold entries below `boundary`.
    ///
    /// ```
    /// use kevy_index::{WindowAudit, WindowShape};
    /// let below_boundary_should_index = 50u64;
    /// let a = WindowAudit::new(1_000, WindowShape::PlainI64, 48);
    /// // the audit's identity: rows below the boundary minus live cold entries
    /// assert_eq!(below_boundary_should_index - a.cold_live, 2, "two rows were lost");
    /// ```
    pub cold_live: u64,
}

impl WindowAudit {
    /// The audit for a tree of `shape` whose eviction boundary is
    /// `boundary`, with `cold_live` live cold entries below it.
    ///
    /// ```
    /// use kevy_index::{WindowAudit, WindowShape};
    /// assert_eq!(WindowAudit::new(0, WindowShape::CompositeLed, 0).shape, WindowShape::CompositeLed);
    /// ```
    pub fn new(boundary: i64, shape: WindowShape, cold_live: u64) -> WindowAudit {
        WindowAudit { boundary, shape, cold_live }
    }
}

pub(crate) fn window_value_of(v: &IndexValue, shape: WindowShape) -> Option<i64> {
    match (shape, v) {
        (WindowShape::PlainI64, IndexValue::I64(i)) => Some(*i),
        (WindowShape::CompositeLed, IndexValue::Str(b)) => {
            let raw = u64::from_be_bytes(b.get(..8)?.try_into().ok()?);
            Some((raw ^ (1 << 63)) as i64)
        }
        _ => None,
    }
}

/// The below-boundary bound for `iter_below` / `split_off_below`,
/// under `shape`. For the composite shape the bound is the target's
/// bare 8-byte first component: any key whose first component sorts
/// below it is below the bound, and a key EQUAL in those 8 bytes is
/// longer and therefore above it — "first column < target", exactly.
///
/// ```
/// use kevy_index::{IndexValue, ValType, WindowShape, order_key, window_bound};
/// let bound = window_bound(100, WindowShape::CompositeLed);
/// let mut at_100 = order_key(ValType::I64, b"100").expect("an i64");
/// at_100.extend_from_slice(b"rest");
/// let mut at_99 = order_key(ValType::I64, b"99").expect("an i64");
/// at_99.extend_from_slice(b"rest");
/// assert!(IndexValue::Str(at_99) < bound && bound < IndexValue::Str(at_100));
/// ```
pub fn window_bound(target: i64, shape: WindowShape) -> IndexValue {
    match shape {
        WindowShape::PlainI64 => IndexValue::I64(target),
        WindowShape::CompositeLed => {
            IndexValue::Str(((target as u64) ^ (1 << 63)).to_be_bytes().to_vec())
        }
    }
}

/// Encode a row's stored `VALUES` into a cold entry's payload:
/// `[n u32][per value: 0 | 1‖len u32‖bytes]`, little-endian. An index
/// that declared no values encodes the EMPTY payload — byte-identical
/// to the a-train's segments, which carried none.
///
/// ```
/// use kevy_index::{decode_seg_values, encode_seg_values};
/// let payload = encode_seg_values(&[Some(b"kyoto"), None]);
/// assert_eq!(decode_seg_values(&payload), Some(vec![Some(b"kyoto".to_vec()), None]));
/// assert!(encode_seg_values(&[]).is_empty());
/// ```
pub fn encode_seg_values(values: &[Option<&[u8]>]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for v in values {
        match v {
            None => out.push(0),
            Some(b) => {
                out.push(1);
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(b);
            }
        }
    }
    out
}

/// Decode a cold entry's payload back to its stored values. The empty
/// payload is the no-values shape; `None` on any malformed frame — a
/// corrupt payload is a refusal upstream, never a guessed row.
///
/// ```
/// use kevy_index::{decode_seg_values, encode_seg_values};
/// assert_eq!(decode_seg_values(&[]), Some(vec![]));
/// let mut payload = encode_seg_values(&[Some(b"v")]);
/// payload.push(0);
/// assert_eq!(decode_seg_values(&payload), None, "trailing bytes are a refusal");
/// ```
pub fn decode_seg_values(payload: &[u8]) -> Option<Vec<Option<Vec<u8>>>> {
    if payload.is_empty() {
        return Some(Vec::new());
    }
    let n = u32::from_le_bytes(payload.get(..4)?.try_into().ok()?) as usize;
    let mut at = 4usize;
    // A count out of the payload is a claim: every entry costs at least
    // its one-byte tag, so `len` bytes cannot honour more than `len` of
    // them. Unbounded, a u32 count reserves up to 4.29e9 elements here.
    let mut out = Vec::with_capacity(n.min(payload.len()));
    for _ in 0..n {
        match payload.get(at)? {
            0 => {
                out.push(None);
                at += 1;
            }
            1 => {
                let len =
                    u32::from_le_bytes(payload.get(at + 1..at + 5)?.try_into().ok()?) as usize;
                out.push(Some(payload.get(at + 5..at + 5 + len)?.to_vec()));
                at += 5 + len;
            }
            _ => return None,
        }
    }
    (at == payload.len()).then_some(out)
}

/// Escape-and-terminate one component into `out`.
fn frame_into(out: &mut Vec<u8>, b: &[u8]) {
    for &c in b {
        if c == 0 {
            out.push(0);
            out.push(0xFF);
        } else {
            out.push(c);
        }
    }
    out.push(0);
    out.push(0);
}

/// Read one framed component; returns (component, rest-after-frame).
fn unframe(b: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0 {
            out.push(b[i]);
            i += 1;
            continue;
        }
        match b.get(i + 1)? {
            0xFF => {
                out.push(0);
                i += 2;
            }
            0x00 => return Some((out, &b[i + 2..])),
            _ => return None,
        }
    }
    None
}

/// The cold-membership bloom: rows whose entries were evicted into
/// segments insert here; the write path consults it before spending a
/// tombstone. A false positive costs one stray tombstone entry
/// (harmless); a false negative cannot happen, which is the property
/// correctness rides on.
///
/// ```
/// use kevy_index::ColdBloom;
/// let mut bloom = ColdBloom::new(1_000);
/// bloom.insert(b"user:1");
/// assert!(bloom.contains(b"user:1"), "no false negatives");
/// let strays = (0..1_000).filter(|i| bloom.contains(format!("other:{i}").as_bytes())).count();
/// assert!(strays < 20, "false positives stay rare");
/// ```
#[derive(Debug)]
pub struct ColdBloom {
    bits: Vec<u64>,
    k: u32,
}

impl ColdBloom {
    /// Sized for ~10 bits per expected item (k=7 → ~1% false-positive
    /// rate at capacity; degrades gracefully past it).
    pub fn new(expected_items: usize) -> Self {
        let words = (expected_items.max(64) * 10 / 64).next_power_of_two();
        Self { bits: vec![0u64; words], k: 7 }
    }

    /// Record that `item` has at least one cold entry.
    pub fn insert(&mut self, item: &[u8]) {
        let (h1, h2) = Self::hashes(item);
        let nbits = (self.bits.len() * 64) as u64;
        for i in 0..self.k as u64 {
            let bit = h1.wrapping_add(i.wrapping_mul(h2)) % nbits;
            self.bits[(bit / 64) as usize] |= 1 << (bit % 64);
        }
    }

    /// Whether `item` MAY have a cold entry (false positives allowed,
    /// false negatives never).
    pub fn contains(&self, item: &[u8]) -> bool {
        let (h1, h2) = Self::hashes(item);
        let nbits = (self.bits.len() * 64) as u64;
        (0..self.k as u64).all(|i| {
            let bit = h1.wrapping_add(i.wrapping_mul(h2)) % nbits;
            self.bits[(bit / 64) as usize] & (1 << (bit % 64)) != 0
        })
    }

    /// FNV-1a under two seeds — double hashing derives the k probes.
    fn hashes(item: &[u8]) -> (u64, u64) {
        let fnv = |seed: u64| {
            let mut h = seed;
            for &b in item {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            h
        };
        (fnv(0xcbf29ce484222325), fnv(0x84222325cbf29ce4) | 1)
    }
}

#[cfg(test)]
#[path = "segcold_tests.rs"]
mod tests;

impl IndexValue {
    /// The order-preserving bytes of one value — [`crate::order_key`]'s
    /// transform, taken from an already-coerced [`IndexValue`]. The pin
    /// test holds the two in lockstep.
    ///
    /// ```
    /// use kevy_index::IndexValue;
    /// assert!(IndexValue::I64(-1).order_bytes() < IndexValue::I64(1).order_bytes());
    /// ```
    pub fn order_bytes(&self) -> Vec<u8> {
        value_order_bytes(self)
    }

    /// The window-column value a tree entry carries, under `shape`.
    /// `None` = the entry cannot carry one (wrong variant / short bytes) —
    /// the caller treats the tree as having no boundary to advance.
    ///
    /// ```
    /// use kevy_index::{IndexValue, WindowShape};
    /// assert_eq!(IndexValue::I64(42).window_value(WindowShape::PlainI64), Some(42));
    /// assert_eq!(IndexValue::I64(42).window_value(WindowShape::CompositeLed), None);
    /// ```
    pub fn window_value(&self, shape: WindowShape) -> Option<i64> {
        window_value_of(self, shape)
    }
}
