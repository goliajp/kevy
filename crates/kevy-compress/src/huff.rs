//! Canonical Huffman over literal bytes — the entropy layer of the
//! compaction level (RFC §3/§7.1: a value that survived until
//! compaction has earned the more expensive encoding).
//!
//! Shape follows zstd rather than deflate: the caller hands us the
//! whole literal block, we code it as one bitstream with a 128-byte
//! canonical-lengths header. Decode builds one flat lookup table and
//! runs a tight loop — the cold-read budget cares about decode, and a
//! table-driven literal pass stays well inside it.
//!
//! Codes are length-limited to [`MAX_LEN`] by the pragmatic heuristic
//! (demote overlong tails onto shorter prefixes and rebalance); the
//! result is a valid canonical code, merely a hair off Huffman-optimal
//! in the pathological skews — which the caller's smallest-wins
//! fallback absorbs by construction.

use alloc::vec;
use alloc::vec::Vec;

use crate::Corrupt;

/// Longest code we emit or accept. 12 bits keeps the decode table at
/// 4096 entries (8 KiB) — L1-resident.
const MAX_LEN: u32 = 12;

/// Header bytes: 256 lengths packed as nibbles.
pub(crate) const HEADER_LEN: usize = 128;

/// Build canonical code lengths for `hist`, length-limited to MAX_LEN.
pub(crate) fn code_lengths(hist: &[u64; 256]) -> [u8; 256] {
    // Huffman via repeated pairing over a sorted worklist. 256 symbols
    // at compaction cadence: simplicity beats a heap.
    let mut nodes: Vec<(u64, Vec<u8>)> = hist
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c > 0)
        .map(|(s, &c)| (c, vec![s as u8]))
        .collect();
    let mut lens = [0u8; 256];
    if nodes.len() == 1 {
        lens[nodes[0].1[0] as usize] = 1;
        return lens;
    }
    while nodes.len() > 1 {
        nodes.sort_by_key(|(c, _)| core::cmp::Reverse(*c));
        let (ca, syms_a) = nodes.pop().expect("len > 1");
        let (cb, syms_b) = nodes.pop().expect("len > 1");
        for &s in syms_a.iter().chain(&syms_b) {
            lens[s as usize] += 1;
        }
        let mut merged = syms_a;
        merged.extend_from_slice(&syms_b);
        nodes.push((ca + cb, merged));
    }
    // Length-limit: push overlong codes up to MAX_LEN, then restore the
    // Kraft sum by lengthening the shallowest leaves that have room.
    let mut kraft: i64 = 0;
    for l in lens.iter_mut().filter(|l| **l > 0) {
        if u32::from(*l) > MAX_LEN {
            *l = MAX_LEN as u8;
        }
        kraft += 1i64 << (MAX_LEN - u32::from(*l));
    }
    let budget = 1i64 << MAX_LEN;
    while kraft > budget {
        // Deepen the deepest-but-not-max symbol with the smallest count
        // contribution: any symbol below MAX_LEN works for validity;
        // pick the one whose deepening frees the most excess first.
        let s = (0..256)
            .filter(|&s| lens[s] > 0 && u32::from(lens[s]) < MAX_LEN)
            .max_by_key(|&s| lens[s])
            .expect("kraft overflow implies a deepenable symbol");
        kraft -= 1i64 << (MAX_LEN - u32::from(lens[s]));
        lens[s] += 1;
        kraft += 1i64 << (MAX_LEN - u32::from(lens[s]));
    }
    lens
}

/// Canonical code assignment from lengths: shorter first, then symbol
/// order — both sides derive identical codes from the header alone.
pub(crate) fn canonical_codes(lens: &[u8; 256]) -> [u16; 256] {
    let mut codes = [0u16; 256];
    let mut next: u32 = 0;
    for bits in 1..=MAX_LEN {
        for s in 0..256 {
            if u32::from(lens[s]) == bits {
                codes[s] = next as u16;
                next += 1;
            }
        }
        next <<= 1;
    }
    codes
}

/// Encode `input` as `[128-byte lengths header][bitstream]`, or `None`
/// when the coded form would not be smaller than the input (never-expand stays a
/// return value at every layer).
pub(crate) fn encode(input: &[u8]) -> Option<Vec<u8>> {
    if input.is_empty() {
        return None;
    }
    let mut hist = [0u64; 256];
    for &b in input {
        hist[b as usize] += 1;
    }
    let lens = code_lengths(&hist);
    let coded_bits = cost_bits(&hist, &lens)?;
    let total = HEADER_LEN + coded_bits.div_ceil(8) as usize;
    if total >= input.len() {
        return None;
    }
    let mut out = Vec::with_capacity(total);
    for pair in lens.chunks_exact(2) {
        out.push(pair[0] | (pair[1] << 4));
    }
    write_bits(input, &lens, &mut out);
    Some(out)
}

/// Total coded bits of `hist` under `lens` — `None` when a needed
/// symbol has no code (an external table may not cover this block).
pub(crate) fn cost_bits(hist: &[u64; 256], lens: &[u8; 256]) -> Option<u64> {
    let mut bits = 0u64;
    for (&c, &l) in hist.iter().zip(lens) {
        if c > 0 {
            if l == 0 {
                return None;
            }
            bits += c * u64::from(l);
        }
    }
    Some(bits)
}

/// Append `input` as an LSB-first bitstream under `lens`.
pub(crate) fn write_bits(input: &[u8], lens: &[u8; 256], out: &mut Vec<u8>) {
    let codes = canonical_codes(lens);
    let (mut acc, mut nbits) = (0u64, 0u32);
    for &b in input {
        let (code, len) = (codes[b as usize], u32::from(lens[b as usize]));
        // LSB-first: append above the bits already pending.
        acc |= u64::from(reverse(code, len)) << nbits;
        nbits += len;
        while nbits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            nbits -= 8;
        }
    }
    if nbits > 0 {
        out.push(acc as u8);
    }
}

/// Bit-reverse the low `len` bits (canonical codes are MSB-first by
/// construction; the stream is LSB-first for cheap reads).
fn reverse(code: u16, len: u32) -> u16 {
    let mut r = 0u16;
    for i in 0..len {
        r |= ((code >> i) & 1) << (len - 1 - i);
    }
    r
}

/// Decode `n` literal bytes from `[header][bitstream]`. Returns the
/// literals and the total bytes consumed from `buf`.
pub(crate) fn decode(buf: &[u8], n: usize) -> Result<(Vec<u8>, usize), Corrupt> {
    let header = buf.get(..HEADER_LEN).ok_or(Corrupt)?;
    let mut lens = [0u8; 256];
    for (i, &b) in header.iter().enumerate() {
        lens[i * 2] = b & 0x0f;
        lens[i * 2 + 1] = b >> 4;
    }
    validate_lens(&lens, n)?;
    let (out, used_bits) = read_bits(&buf[HEADER_LEN..], &lens, n)?;
    Ok((out, HEADER_LEN + used_bits.div_ceil(8) as usize))
}

/// Kraft validation: an over-full code space lets two codes alias;
/// reject instead of guessing.
pub(crate) fn validate_lens(lens: &[u8; 256], n: usize) -> Result<(), Corrupt> {
    let kraft: u64 = lens
        .iter()
        .filter(|&&l| l > 0)
        .map(|&l| 1u64 << (MAX_LEN - u32::from(l)))
        .sum();
    if kraft > (1u64 << MAX_LEN) || (n > 0 && kraft == 0) {
        return Err(Corrupt);
    }
    Ok(())
}

/// Decode `n` symbols from an LSB-first bitstream under `lens`;
/// returns the bytes and the exact bits consumed.
pub(crate) fn read_bits(stream: &[u8], lens: &[u8; 256], n: usize) -> Result<(Vec<u8>, u64), Corrupt> {
    let codes = canonical_codes(lens);
    // Flat table: index = next MAX_LEN reversed bits -> (symbol, len).
    let mut table = vec![(0u8, 0u8); 1 << MAX_LEN];
    for s in 0..256 {
        let l = u32::from(lens[s]);
        if l == 0 {
            continue;
        }
        let base = reverse(codes[s], l) as usize;
        let step = 1usize << l;
        let mut ix = base;
        while ix < (1 << MAX_LEN) {
            table[ix] = (s as u8, l as u8);
            ix += step;
        }
    }
    let mut out = Vec::with_capacity(n);
    let (mut acc, mut nbits, mut pos) = (0u64, 0u32, 0usize);
    let mut used_bits: u64 = 0;
    for _ in 0..n {
        while nbits < MAX_LEN && pos < stream.len() {
            acc |= u64::from(stream[pos]) << nbits;
            pos += 1;
            nbits += 8;
        }
        let (sym, l) = table[(acc & ((1 << MAX_LEN) - 1)) as usize];
        if l == 0 || u32::from(l) > nbits {
            return Err(Corrupt);
        }
        out.push(sym);
        acc >>= l;
        nbits -= u32::from(l);
        used_bits += u64::from(l);
    }
    // Consumed length by BIT accounting, not by prefetch position: the
    // reader may have pulled bytes past the last code (harmless — they
    // are the caller's next section), and the section boundary must
    // land exactly.
    Ok((out, used_bits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn roundtrips_and_shrinks_text() {
        let mut text = Vec::new();
        for i in 0..300 {
            text.extend_from_slice(format!("record {i}: the quick brown fox\n").as_bytes());
        }
        let coded = encode(&text).expect("text must shrink");
        assert!(coded.len() < text.len());
        let (back, used) = decode(&coded, text.len()).unwrap();
        assert_eq!(back, text);
        assert_eq!(used, coded.len());
    }

    #[test]
    fn single_symbol_and_skewed_inputs_roundtrip() {
        for input in [vec![7u8; 500], (0..=255u8).cycle().take(3000).collect::<Vec<_>>()] {
            if let Some(coded) = encode(&input) {
                let (back, _) = decode(&coded, input.len()).unwrap();
                assert_eq!(back, input);
            }
        }
    }

    #[test]
    fn corrupt_headers_reject() {
        let text = b"aaaaabbbbbcccccdddddaaaaabbbbb".repeat(20);
        let mut coded = encode(&text).unwrap();
        // Over-full Kraft sum: every symbol claims a 1-bit code.
        for b in coded.iter_mut().take(HEADER_LEN) {
            *b = 0x11;
        }
        assert!(decode(&coded, text.len()).is_err());
        assert!(decode(&[0u8; 10], 5).is_err(), "short header must reject");
    }
}
