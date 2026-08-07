//! The fast-level encoder: single-probe hash match finder over
//! `dict ++ input` virtual history, LZ4-style token stream out.
//!
//! spg-lzss's measured limits named what to avoid (RFC §4.1): its
//! every-position window walk is the 50 MiB/s, its 4 KiB window blinds
//! it past one page, its 18-byte match cap shreds long runs. Here: one
//! hash probe per position, 64 KiB of reachable history extended
//! further by the dictionary, unbounded match lengths.

use alloc::vec;
use alloc::vec::Vec;

use crate::{MAX_OFFSET, TAG_LZ, TAG_LZ_DICT};

/// Inputs shorter than this cannot contain a minimum match plus a
/// saving; the caller stores them raw without trying.
pub(crate) const MIN_INPUT: usize = 8;

/// Minimum match the token format can express (nibble 0 = 4 bytes).
const MIN_MATCH: usize = 4;

/// Hash-table entries. 4096 keeps the table L1-resident; each entry is
/// one virtual position (the single-probe discipline).
const HT_BITS: u32 = 12;

/// The last positions of the input are emitted as literals without
/// probing: a match must not run into the final bytes the decoder's
/// bulk copies want as plain literals, and probing them cannot save
/// enough to matter.
const TAIL_LITERALS: usize = 5;

/// One virtual byte: positions below `dict.len()` read the dictionary,
/// the rest read the input.
#[inline]
fn vbyte(dict: &[u8], input: &[u8], v: usize) -> u8 {
    if v < dict.len() { dict[v] } else { input[v - dict.len()] }
}

#[inline]
fn hash4(dict: &[u8], input: &[u8], v: usize) -> usize {
    let w = u32::from_le_bytes([
        vbyte(dict, input, v),
        vbyte(dict, input, v + 1),
        vbyte(dict, input, v + 2),
        vbyte(dict, input, v + 3),
    ]);
    (w.wrapping_mul(2_654_435_761) >> (32 - HT_BITS)) as usize
}

/// Try to LZ-encode `input` into `out` (payload only, no header).
/// Returns `(tag, true)` on success; `(_, false)` when the payload
/// would not be smaller than the raw bytes — the K2 discipline is a
/// return value, not a hope.
pub(crate) fn try_lz(dict: &[u8], input: &[u8], out: &mut Vec<u8>) -> (u8, bool) {
    let d = dict.len().min(MAX_OFFSET);
    let dict = &dict[dict.len() - d..];
    let mut table = vec![0u32; 1 << HT_BITS];
    seed(dict, &mut table);
    let (mut v, end) = (d, d + input.len());
    let probe_end = end - TAIL_LITERALS.min(input.len());
    let mut lit_start = d;
    let mut used_dict = false;
    while v + MIN_MATCH <= probe_end {
        let h = hash4(dict, input, v);
        let cand = table[h] as usize;
        table[h] = v as u32;
        let dist = v - cand;
        if cand < v && dist <= MAX_OFFSET && matches4(dict, input, cand, v) {
            let len = extend(dict, input, cand, v, end);
            used_dict |= cand < d;
            emit(input, d, lit_start..v, dist, len, out);
            if out.len() >= input.len() {
                return (TAG_LZ, false);
            }
            v += len;
            lit_start = v;
        } else {
            v += 1;
        }
    }
    emit_tail(input, d, lit_start..end, out);
    let tag = if used_dict { TAG_LZ_DICT } else { TAG_LZ };
    (tag, out.len() < input.len())
}

/// Pre-hash the dictionary so its positions are reachable from the
/// first input byte — the cross-value capture K4 is about.
fn seed(dict: &[u8], table: &mut [u32]) {
    if dict.len() < MIN_MATCH {
        return;
    }
    for v in 0..=dict.len() - MIN_MATCH {
        let h = hash4(dict, &[], v);
        table[h] = v as u32;
    }
}

#[inline]
fn matches4(dict: &[u8], input: &[u8], a: usize, b: usize) -> bool {
    (0..MIN_MATCH).all(|i| vbyte(dict, input, a + i) == vbyte(dict, input, b + i))
}

/// Longest common run from `(a, b)`, bounded by the end of input.
fn extend(dict: &[u8], input: &[u8], a: usize, b: usize, end: usize) -> usize {
    let mut len = 0;
    while b + len < end && vbyte(dict, input, a + len) == vbyte(dict, input, b + len) {
        len += 1;
    }
    len
}

/// One sequence: token, literal run, offset, match length.
fn emit(input: &[u8], d: usize, lits: core::ops::Range<usize>, dist: usize, len: usize, out: &mut Vec<u8>) {
    let lit_len = lits.len();
    let mat = len - MIN_MATCH;
    out.push(((lit_len.min(15) as u8) << 4) | mat.min(15) as u8);
    push_ext(out, lit_len, 15);
    out.extend_from_slice(&input[lits.start - d..lits.end - d]);
    out.extend_from_slice(&(dist as u16).to_le_bytes());
    push_ext(out, mat, 15);
}

/// The closing literal run: a token with no offset after it. The
/// decoder knows it is last because the payload ends here.
fn emit_tail(input: &[u8], d: usize, lits: core::ops::Range<usize>, out: &mut Vec<u8>) {
    let lit_len = lits.len();
    out.push((lit_len.min(15) as u8) << 4);
    push_ext(out, lit_len, 15);
    out.extend_from_slice(&input[lits.start - d..lits.end - d]);
}

/// 255-continuation length extension (LZ4's shape).
fn push_ext(out: &mut Vec<u8>, value: usize, nibble_max: usize) {
    if value < nibble_max {
        return;
    }
    let mut rest = value - nibble_max;
    while rest >= 255 {
        out.push(255);
        rest -= 255;
    }
    out.push(rest as u8);
}
