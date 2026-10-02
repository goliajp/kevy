//! The sparse encoding: the registers as runs, one- or two-byte opcodes.
//!
//! - `00xxxxxx` ZERO: 1 to 64 registers at 0.
//! - `01xxxxxx yyyyyyyy` XZERO: 1 to 16384 registers at 0.
//! - `1vvvvvxx` VAL: 1 to 4 registers at a value of 1 to 32.
//!
//! Setting a register rewrites only the run that holds it, then merges
//! equal neighbours in a short window after the run before it, so the
//! bytes depend on the order registers were set in, as Redis's do.

#[cfg(not(feature = "std"))]
use crate::nostd_prelude::*;

use super::{Corrupt, HDR, REGISTERS};

pub(super) const VAL_MAX_VALUE: u8 = 32;
const VAL_MAX_LEN: u32 = 4;
const ZERO_MAX_LEN: u32 = 64;

#[derive(Clone, Copy)]
pub(super) enum Op {
    Zero(u32),
    Val(u8, u32),
}

/// The opcode at `at`, and its length in bytes.
pub(super) fn decode(b: &[u8], at: usize) -> Result<(Op, usize), Corrupt> {
    let c = b[at];
    Ok(match c >> 6 {
        0 => (Op::Zero(u32::from(c & 0x3f) + 1), 1),
        1 => {
            let lo = *b.get(at + 1).ok_or(Corrupt)?;
            (Op::Zero(((u32::from(c & 0x3f) << 8) | u32::from(lo)) + 1), 2)
        }
        _ => (Op::Val(((c >> 2) & 0x1f) + 1, u32::from(c & 3) + 1), 1),
    })
}

fn is_val(c: u8) -> bool {
    c & 0x80 != 0
}

fn val_byte(value: u8, len: u32) -> u8 {
    0x80 | ((value - 1) << 2) | (len - 1) as u8
}

fn push_zero(out: &mut Vec<u8>, len: u32) {
    if len <= ZERO_MAX_LEN {
        out.push((len - 1) as u8);
    } else {
        let l = len - 1;
        out.extend_from_slice(&[0x40 | (l >> 8) as u8, l as u8]);
    }
}

/// An empty set of registers: one XZERO over all of them.
pub(super) fn empty_runs(out: &mut Vec<u8>) {
    push_zero(out, REGISTERS as u32);
}

/// What setting a register did.
pub(super) enum Set {
    Unchanged,
    Updated,
    /// The value or the bytes outgrow the encoding: go dense first.
    Promote,
}

/// Raise register `index` to `count` in the sparse `buf` (header
/// included) of at most `max_bytes` bytes.
pub(super) fn set(
    buf: &mut Vec<u8>,
    index: u32,
    count: u8,
    max_bytes: usize,
) -> Result<Set, Corrupt> {
    if count > VAL_MAX_VALUE {
        return Ok(Set::Promote);
    }
    let (mut p, mut prev, mut first, mut found) = (HDR, None, 0u32, None);
    while p < buf.len() {
        let (op, oplen) = decode(buf, p)?;
        let span = match op {
            Op::Zero(n) | Op::Val(_, n) => n,
        };
        if index < first + span {
            found = Some((op, oplen, span));
            break;
        }
        prev = Some(p);
        p += oplen;
        first += span;
    }
    let (op, oplen, span) = found.ok_or(Corrupt)?;
    match op {
        Op::Val(v, _) if v >= count => return Ok(Set::Unchanged),
        Op::Val(_, 1) => buf[p] = val_byte(count, 1),
        Op::Zero(1) if oplen == 1 => buf[p] = val_byte(count, 1),
        _ => {
            let seq = split(op, first, span, index, count);
            if seq.len() > oplen && buf.len() + seq.len() - oplen > max_bytes {
                return Ok(Set::Promote);
            }
            buf.splice(p..p + oplen, seq);
        }
    }
    merge(buf, prev.unwrap_or(HDR));
    Ok(Set::Updated)
}

/// The run `first .. first + span` rewritten with `index` at `count`.
fn split(op: Op, first: u32, span: u32, index: u32, count: u8) -> Vec<u8> {
    let last = first + span - 1;
    let mut seq = Vec::with_capacity(5);
    let around = |seq: &mut Vec<u8>, len: u32| match op {
        Op::Zero(_) => push_zero(seq, len),
        Op::Val(v, _) => seq.push(val_byte(v, len)),
    };
    if index != first {
        around(&mut seq, index - first);
    }
    seq.push(val_byte(count, 1));
    if index != last {
        around(&mut seq, last - index);
    }
    seq
}

/// Merge equal adjacent VALs that fit one opcode, over at most five
/// opcodes from `from`.
fn merge(buf: &mut Vec<u8>, from: usize) {
    let (mut p, mut scan) = (from, 5);
    while p < buf.len() && scan > 0 {
        scan -= 1;
        let c = buf[p];
        if !is_val(c) {
            p += if c >> 6 == 1 { 2 } else { 1 };
            continue;
        }
        if let Some(&n) = buf.get(p + 1)
            && is_val(n)
            && (c >> 2) & 0x1f == (n >> 2) & 0x1f
        {
            let len = u32::from(c & 3) + u32::from(n & 3) + 2;
            if len <= VAL_MAX_LEN {
                buf[p + 1] = val_byte(((c >> 2) & 0x1f) + 1, len);
                buf.remove(p);
                continue;
            }
        }
        p += 1;
    }
}
