//! HyperLogLog, stored as a string in Redis's own layout so the bytes are
//! the same ones Redis would hold: a 16-byte header (`HYLL`, the encoding,
//! three spare bytes, an 8-byte cached cardinality whose top bit marks it
//! stale) followed by 16384 six-bit registers, sparse or dense.

mod estimate;
mod murmur;
mod sparse;

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;
use alloc::borrow::Cow;

use crate::{Store, StoreError};

const P: u32 = 14;
const Q: usize = 64 - P as usize;
const REGISTERS: usize = 1 << P;
const HDR: usize = 16;
const DENSE_LEN: usize = HDR + REGISTERS * 6 / 8;
const SPARSE: u8 = 1;
const DENSE: u8 = 0;
/// Redis's default `hll-sparse-max-bytes`.
const SPARSE_MAX_BYTES: usize = 3000;

/// A sparse encoding whose runs do not add up.
pub(super) struct Corrupt;

impl From<Corrupt> for StoreError {
    fn from(_: Corrupt) -> StoreError {
        StoreError::HllCorrupt
    }
}

/// The register an element lands in and the count it carries there.
fn place(element: &[u8]) -> (u32, u8) {
    let hash = murmur::murmur64a(element, 0xadc8_3b19);
    let index = (hash & (REGISTERS as u64 - 1)) as u32;
    let rest = (hash >> P) | (1 << Q);
    (index, rest.trailing_zeros() as u8 + 1)
}

fn new_sparse() -> Vec<u8> {
    let mut v = b"HYLL".to_vec();
    v.push(SPARSE);
    v.extend_from_slice(&[0; 10]);
    v.push(0x80);
    sparse::empty_runs(&mut v);
    v
}

/// The bytes when they are a HyperLogLog Redis would accept.
fn check(b: &[u8]) -> Result<(), StoreError> {
    let ok = b.len() >= HDR
        && &b[..4] == b"HYLL"
        && (b[4] == SPARSE || (b[4] == DENSE && b.len() == DENSE_LEN));
    if ok { Ok(()) } else { Err(StoreError::NotHll) }
}

fn cached(b: &[u8]) -> Option<u64> {
    (b[15] & 0x80 == 0).then(|| u64::from_le_bytes(b[8..16].try_into().expect("eight bytes")))
}

fn invalidate(b: &mut [u8]) {
    b[15] |= 0x80;
}

fn dense_get(regs: &[u8], i: usize) -> u8 {
    let (byte, bit) = (i * 6 / 8, (i * 6 % 8) as u32);
    let lo = u16::from(regs[byte]) >> bit;
    let hi = u16::from(regs.get(byte + 1).copied().unwrap_or(0)) << (8 - bit);
    ((lo | hi) & 63) as u8
}

fn dense_set(regs: &mut [u8], i: usize, v: u8) {
    let (byte, bit) = (i * 6 / 8, (i * 6 % 8) as u32);
    regs[byte] = (regs[byte] & !(63u16 << bit) as u8) | (u16::from(v) << bit) as u8;
    // the register runs into the next byte only from bit 3 on
    if bit > 2 {
        let next = &mut regs[byte + 1];
        *next = (*next & !(63u8 >> (8 - bit))) | (v >> (8 - bit));
    }
}

/// Each register's value, as runs `(value, len)` in register order.
fn for_each_run(b: &[u8], mut f: impl FnMut(u8, u32)) -> Result<(), Corrupt> {
    if b[4] == DENSE {
        (0..REGISTERS).for_each(|i| f(dense_get(&b[HDR..], i), 1));
        return Ok(());
    }
    let (mut p, mut total) = (HDR, 0u32);
    while p < b.len() {
        let (op, n) = sparse::decode(b, p)?;
        let (v, len) = match op {
            sparse::Op::Zero(len) => (0, len),
            sparse::Op::Val(v, len) => (v, len),
        };
        f(v, len);
        total += len;
        p += n;
    }
    if total == REGISTERS as u32 { Ok(()) } else { Err(Corrupt) }
}

fn to_dense(b: &[u8]) -> Result<Vec<u8>, Corrupt> {
    let mut out = b[..HDR].to_vec();
    out[4] = DENSE;
    out.resize(DENSE_LEN, 0);
    let mut i = 0;
    for_each_run(b, |v, len| {
        for _ in 0..len {
            if v != 0 && i < REGISTERS {
                dense_set(&mut out[HDR..], i, v);
            }
            i += 1;
        }
    })?;
    Ok(out)
}

/// Raise register `index` to `count`, going dense when sparse will not do.
fn raise(buf: &mut Vec<u8>, index: u32, count: u8) -> Result<bool, Corrupt> {
    if buf[4] == SPARSE {
        match sparse::set(buf, index, count, SPARSE_MAX_BYTES)? {
            sparse::Set::Unchanged => return Ok(false),
            sparse::Set::Updated => return Ok(true),
            sparse::Set::Promote => *buf = to_dense(buf)?,
        }
    }
    let regs = &mut buf[HDR..];
    if dense_get(regs, index as usize) >= count {
        return Ok(false);
    }
    dense_set(regs, index as usize, count);
    Ok(true)
}

fn histogram(b: &[u8]) -> Result<[u32; 64], Corrupt> {
    let mut h = [0u32; 64];
    for_each_run(b, |v, len| h[v as usize] += len)?;
    Ok(h)
}

/// Fold `b`'s registers into `max`, each the larger of the two.
fn merge_into(max: &mut [u8], b: &[u8]) -> Result<(), Corrupt> {
    let mut i = 0usize;
    for_each_run(b, |v, len| {
        for _ in 0..len {
            if let Some(m) = max.get_mut(i) {
                *m = (*m).max(v);
            }
            i += 1;
        }
    })
}

impl Store {
    /// The HyperLogLog at `key`: `None` absent.
    fn hll_bytes(&mut self, key: &[u8]) -> Result<Option<Cow<'_, [u8]>>, StoreError> {
        let bytes = self.get(key)?;
        if let Some(b) = &bytes {
            check(b)?;
        }
        Ok(bytes)
    }
}

mod commands;

#[cfg(test)]
mod tests {
    use super::*;

    // bytes read back with GET from Redis 8.10 after the same commands
    #[test]
    fn the_bytes_are_redis_bytes() {
        assert_eq!(place(b"a"), (12711, 2));
        let mut s = Store::new();
        s.pfadd(b"h", &[]).unwrap();
        assert_eq!(
            s.get(b"h").unwrap().unwrap().as_ref(),
            b"HYLL\x01\0\0\0\0\0\0\0\0\0\0\x80\x7f\xff"
        );
        s.pfadd(b"h", &[b"a"]).unwrap();
        assert_eq!(
            s.get(b"h").unwrap().unwrap().as_ref(),
            b"HYLL\x01\0\0\0\0\0\0\0\0\0\0\x80q\xa6\x84NW"
        );
        s.pfadd(b"h", &[b"b", b"c"]).unwrap();
        let three = b"HYLL\x01\0\0\0\0\0\0\0\0\0\0\x80`\xf3\x80P\xb1\x84K\xfb\x80BZ";
        assert_eq!(s.get(b"h").unwrap().unwrap().as_ref(), three);
        assert_eq!(s.pfcount(&[b"h"]).unwrap(), (3, true));
        let cached = b"HYLL\x01\0\0\0\x03\0\0\0\0\0\0\0`\xf3\x80P\xb1\x84K\xfb\x80BZ";
        assert_eq!(s.get(b"h").unwrap().unwrap().as_ref(), cached);
        s.pfadd(b"h", &[b"d"]).unwrap();
        let four = b"HYLL\x01\0\0\0\x03\0\0\0\0\0\0\x80\\{\x80Dv\x80P\xb1\x84K\xfb\x80BZ";
        assert_eq!(s.get(b"h").unwrap().unwrap().as_ref(), four);
    }
}
