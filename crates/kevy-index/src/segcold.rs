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

/// The order-preserving bytes of one value — [`crate::order_key`]'s
/// transform, taken from an already-coerced [`IndexValue`]. The pin
/// test holds the two in lockstep.
pub fn value_order_bytes(v: &IndexValue) -> Vec<u8> {
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
pub fn seg_key(v: &IndexValue, row_key: &[u8]) -> Vec<u8> {
    let vb = value_order_bytes(v);
    let mut out = Vec::with_capacity(vb.len() + row_key.len() + 4);
    frame_into(&mut out, &vb);
    frame_into(&mut out, row_key);
    out
}

/// Decode a segment key back to `(value, row_key)`. `ty` is the
/// index's declared type (the key does not carry it). `None` on any
/// malformed frame — a corrupt key is a refusal upstream, never a
/// guessed entry.
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
