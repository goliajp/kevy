//! The encoder: one match finder, two serializers.
//!
//! Pass 1 collects sequences (single-probe hash finder over
//! `dict ++ input` virtual history — spg-lzss's measured limits named
//! what to avoid: no per-position window walks, 64 KiB reach, unbounded
//! lengths). Pass 2 serializes them either as the **fast level**
//! (LZ4-shape, literals inline — the demote path) or the **high
//! level** (literals pulled out into one Huffman-coded block, zstd's
//! shape — the compaction path, where a value has earned the more
//! expensive encoding). Same finder, same sequences, one format
//! decision apart — RFC §7.1's "changes one argument, not the design".

use alloc::vec;
use alloc::vec::Vec;

use crate::{MAX_OFFSET, TAG_LZ, TAG_LZ_DICT, TAG_LZH, TAG_LZH_DICT};

/// Inputs shorter than this cannot contain a minimum match plus a
/// saving; the caller stores them raw without trying.
pub(crate) const MIN_INPUT: usize = 8;

/// Minimum match the token format can express (nibble 0 = 4 bytes).
const MIN_MATCH: usize = 4;

/// Hash-table entries. 4096 keeps the table L1-resident; each entry is
/// one virtual position (the single-probe discipline).
const HT_BITS: u32 = 12;

/// The last positions of the input are emitted as literals without
/// probing; probing them cannot save enough to matter.
const TAIL_LITERALS: usize = 5;

/// One collected sequence: a literal run (virtual positions), then a
/// back-reference.
struct Seq {
    lits: core::ops::Range<usize>,
    dist: usize,
    len: usize,
}

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

/// Pass 1: the match finder. Returns the sequences, the closing
/// literal run, and whether any match reached into the dictionary.
fn collect(dict: &[u8], input: &[u8]) -> (Vec<Seq>, core::ops::Range<usize>, bool) {
    let d = dict.len();
    let mut table = vec![0u32; 1 << HT_BITS];
    seed(dict, &mut table);
    let (mut v, end) = (d, d + input.len());
    let probe_end = end - TAIL_LITERALS.min(input.len());
    let mut lit_start = d;
    let mut used_dict = false;
    let mut seqs = Vec::new();
    while v + MIN_MATCH <= probe_end {
        let h = hash4(dict, input, v);
        let cand = table[h] as usize;
        table[h] = v as u32;
        let dist = v - cand;
        if cand < v && dist <= MAX_OFFSET && matches4(dict, input, cand, v) {
            let len = extend(dict, input, cand, v, end);
            used_dict |= cand < d;
            seqs.push(Seq { lits: lit_start..v, dist, len });
            v += len;
            lit_start = v;
        } else {
            v += 1;
        }
    }
    (seqs, lit_start..end, used_dict)
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

/// Fast-level serializer: LZ4-shape, literals inline. `None` once the
/// output reaches `cap` (raw or a smaller rival wins).
fn serialize_fast(
    dict: &[u8],
    input: &[u8],
    seqs: &[Seq],
    tail: &core::ops::Range<usize>,
    cap: usize,
) -> Option<Vec<u8>> {
    let d = dict.len();
    let mut out = Vec::with_capacity(input.len().min(cap) + 16);
    for s in seqs {
        let lit_len = s.lits.len();
        let mat = s.len - MIN_MATCH;
        out.push(((lit_len.min(15) as u8) << 4) | mat.min(15) as u8);
        push_ext(&mut out, lit_len);
        out.extend_from_slice(&input[s.lits.start - d..s.lits.end - d]);
        out.extend_from_slice(&(s.dist as u16).to_le_bytes());
        push_ext(&mut out, mat);
        if out.len() >= cap {
            return None;
        }
    }
    let lit_len = tail.len();
    out.push((lit_len.min(15) as u8) << 4);
    push_ext(&mut out, lit_len);
    out.extend_from_slice(&input[tail.start - d..tail.end - d]);
    (out.len() < cap).then_some(out)
}

/// High-level serializer: `[varint lit_total][flag][literal block]
/// [sequence stream]` — literals pulled out of the stream and
/// Huffman-coded as one block when that wins (flag 1), raw otherwise
/// (flag 0: K2 holds at every layer).
fn serialize_high(
    dict: &[u8],
    lens: Option<&[u8; 256]>,
    input: &[u8],
    seqs: &[Seq],
    tail: &core::ops::Range<usize>,
    cap: usize,
) -> Option<Vec<u8>> {
    let d = dict.len();
    let mut lits = Vec::new();
    for s in seqs {
        lits.extend_from_slice(&input[s.lits.start - d..s.lits.end - d]);
    }
    lits.extend_from_slice(&input[tail.start - d..tail.end - d]);
    let mut out = Vec::with_capacity(input.len().min(cap) + 16);
    crate::push_varint(&mut out, lits.len());
    // Three literal encodings compete; smallest wins. Flag 2 (the
    // file-scoped shared table) has no per-record header, which is
    // what lets entropy coding engage at 400 B.
    let shared_bytes = lens.and_then(|l| {
        let mut hist = [0u64; 256];
        for &b in &lits {
            hist[b as usize] += 1;
        }
        crate::huff::cost_bits(&hist, l).map(|bits| bits.div_ceil(8) as usize)
    });
    let inline = crate::huff::encode(&lits);
    let inline_len = inline.as_ref().map_or(usize::MAX, Vec::len);
    let shared_len = shared_bytes.unwrap_or(usize::MAX);
    if shared_len < inline_len && shared_len < lits.len() {
        out.push(2);
        crate::huff::write_bits(&lits, lens.expect("shared_len set"), &mut out);
    } else if let Some(coded) = inline.filter(|c| c.len() < lits.len()) {
        out.push(1);
        out.extend_from_slice(&coded);
    } else {
        out.push(0);
        out.extend_from_slice(&lits);
    }
    for s in seqs {
        let lit_len = s.lits.len();
        let mat = s.len - MIN_MATCH;
        out.push(((lit_len.min(15) as u8) << 4) | mat.min(15) as u8);
        push_ext(&mut out, lit_len);
        out.extend_from_slice(&(s.dist as u16).to_le_bytes());
        push_ext(&mut out, mat);
        if out.len() >= cap {
            return None;
        }
    }
    let lit_len = tail.len();
    out.push((lit_len.min(15) as u8) << 4);
    push_ext(&mut out, lit_len);
    (out.len() < cap).then_some(out)
}

/// Try to LZ-encode `input` into `out` at the fast level (payload
/// only, no header). `(_, false)` when raw wins — the K2 discipline is
/// a return value, not a hope.
pub(crate) fn try_lz(dict: &[u8], input: &[u8], out: &mut Vec<u8>) -> (u8, bool) {
    let d = dict.len().min(MAX_OFFSET);
    let dict = &dict[dict.len() - d..];
    let (seqs, tail, used_dict) = collect(dict, input);
    match serialize_fast(dict, input, &seqs, &tail, input.len()) {
        Some(payload) => {
            *out = payload;
            (if used_dict { TAG_LZ_DICT } else { TAG_LZ }, true)
        }
        None => (TAG_LZ, false),
    }
}

/// The compaction level: collect once, serialize both ways, keep the
/// smallest of high / fast / raw.
pub(crate) fn try_high(
    dict: &[u8],
    lens: Option<&[u8; 256]>,
    input: &[u8],
    out: &mut Vec<u8>,
) -> (u8, bool) {
    let d = dict.len().min(MAX_OFFSET);
    let dict = &dict[dict.len() - d..];
    let (seqs, tail, used_dict) = collect(dict, input);
    let fast = serialize_fast(dict, input, &seqs, &tail, input.len());
    let cap = fast.as_ref().map_or(input.len(), Vec::len);
    if let Some(high) = serialize_high(dict, lens, input, &seqs, &tail, cap) {
        *out = high;
        return (if used_dict { TAG_LZH_DICT } else { TAG_LZH }, true);
    }
    match fast {
        Some(payload) => {
            *out = payload;
            (if used_dict { TAG_LZ_DICT } else { TAG_LZ }, true)
        }
        None => (TAG_LZ, false),
    }
}

/// 255-continuation length extension (LZ4's shape).
fn push_ext(out: &mut Vec<u8>, value: usize) {
    if value < 15 {
        return;
    }
    let mut rest = value - 15;
    while rest >= 255 {
        out.push(255);
        rest -= 255;
    }
    out.push(rest as u8);
}
