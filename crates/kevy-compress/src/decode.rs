//! The decoder: token stream in, bytes out, every step bounds-checked.
//!
//! Speed comes from shape, not from unsafe: literal runs are
//! `extend_from_slice` (memcpy-class), non-overlapping matches are one
//! `extend_from_within`, and only the two genuinely irregular cases —
//! an overlapping match (offset < length: a repeating pattern by
//! definition) and a match crossing the dictionary/output boundary —
//! fall back to stepwise copies.
//!
//! That was once written as "an order of magnitude above the ~1 GB/s
//! budget floor", on the strength of `examples/k1_sanity`. That example
//! trains the dictionary on the very value it then compresses, so the
//! 4 KiB input becomes a 100-byte frame — a 41x ratio — and the decode
//! it times is one long match out of the dictionary. No stored value is
//! ever its own training sample.
//!
//! `examples/decode_budget` measures held-out values against a
//! dictionary trained on a different sample, which is the shape
//! `kevy-vlog` produces. On that shape the floor is met at 2.079 GB/s
//! fast and 1.265 through compaction — through `Dict`. Through
//! `decode`, which reparses the dictionary per call, it is 0.543 and
//! 0.045. See `lib.rs`'s note on the decode requirement.
//!
//! A frame that walks outside its promised bounds at any point is
//! rejected with [`Corrupt`]. That is a bound on the walk, and it is
//! worth being exact about what it is not.
//!
//! **This layer does not detect corruption, and is not where that
//! belongs.** A frame carries no checksum. A flipped bit that leaves
//! every offset and length in range produces a different, structurally
//! valid token stream, and the decode returns it — measured on a held-out
//! JSON frame, 38% of single-byte flips decode to output of exactly the
//! right length and the wrong contents. Integrity is `kevy-vlog`'s
//! per-record `crc32c`, checked in `verify_image` **before** the frame
//! reaches here; `lib.rs`'s decode says the same thing where it retries a
//! 5.0.0-era tag ("the record's CRC already vouched for the bytes").
//!
//! What this layer does guarantee, for arbitrary bytes: it never reads
//! outside a slice, never panics, never reserves on an unvalidated
//! length, and never returns more than `orig_len`. Truncation it does
//! catch, because a short frame cannot satisfy `out.len() == orig_len`.
//! `a_bit_flip_can_decode_to_something_else` in `tests.rs` holds the
//! distinction, so that the header cannot drift back into promising
//! integrity it does not provide.
//!
//! The header did promise it, in those words, until 2026-09-10. What
//! checked the promise was four sampled byte positions with one bit
//! pattern each, and on its own fixture 894 of 2356 positions mis-decode
//! — the four it happened to pick were not among them.

use alloc::vec::Vec;

use crate::Corrupt;

/// How much a frame could possibly expand to, as an upper bound on the
/// initial reservation — not a limit on the decode, which enforces
/// `orig_len` itself at every step.
///
/// `orig_len` is a number read out of the frame, and until the decode has
/// walked the frame it is a claim. Reserving on the claim let twelve bytes
/// ask for 2.86 GB (`decode_arbitrary` OOM, first run of a target that had
/// been in the tree unrun). The real ceiling comes from the format: a match
/// costs at least a token and two offset bytes, and each further length
/// byte adds at most 255 (`read_len`), so output per payload byte tends to
/// 255 from below. 256 with a slack for the 4 + 15 base is a safe
/// over-estimate — every honest frame still reserves exactly `orig_len`.
pub(crate) fn reserve_for(orig_len: usize, payload_len: usize) -> usize {
    orig_len.min(payload_len.saturating_mul(256).saturating_add(1024))
}

/// Decode an LZ payload whose history is `dict ++ output`.
pub(crate) fn lz(dict: &[u8], payload: &[u8], orig_len: usize) -> Result<Vec<u8>, Corrupt> {
    let mut out = Vec::with_capacity(reserve_for(orig_len, payload.len()));
    let mut p = 0;
    loop {
        let token = *payload.get(p).ok_or(Corrupt)?;
        p += 1;
        let lit_len = read_len(payload, &mut p, (token >> 4) as usize)?;
        let lit_end = p.checked_add(lit_len).ok_or(Corrupt)?;
        if lit_end > payload.len() || out.len() + lit_len > orig_len {
            return Err(Corrupt);
        }
        out.extend_from_slice(&payload[p..lit_end]);
        p = lit_end;
        if p == payload.len() {
            break;
        }
        let off_bytes: [u8; 2] =
            payload.get(p..p + 2).ok_or(Corrupt)?.try_into().map_err(|_| Corrupt)?;
        let dist = usize::from(u16::from_le_bytes(off_bytes));
        p += 2;
        let len = read_len(payload, &mut p, (token & 0x0f) as usize)? + 4;
        if dist == 0 || dist > out.len() + dict.len() || out.len() + len > orig_len {
            return Err(Corrupt);
        }
        copy_match(dict, &mut out, dist, len);
    }
    if out.len() == orig_len { Ok(out) } else { Err(Corrupt) }
}

/// The literal section by flag: raw slice (0), inline-table Huffman
/// (1), or the dictionary's file-scoped table (2 — a frame that needs
/// it cannot decode without its dictionary). Returns the literals and
/// the section's byte length.
fn read_literal_block<'a>(
    flag: u8,
    rest: &'a [u8],
    lit_total: usize,
    lens: Option<&[u8; 256]>,
    table: Option<&crate::huff::DecodeTable>,
) -> Result<(alloc::borrow::Cow<'a, [u8]>, usize), Corrupt> {
    match flag {
        0 => {
            let l = rest.get(..lit_total).ok_or(Corrupt)?;
            Ok((alloc::borrow::Cow::Borrowed(l), lit_total))
        }
        1 => {
            let (l, used) = crate::huff::decode(rest, lit_total)?;
            Ok((alloc::borrow::Cow::Owned(l), used))
        }
        2 => {
            // The dictionary-carried table. Built once per file when the
            // caller holds a `Dict`; built here from the lengths only
            // when it does not, which is the path `decode` takes.
            let (out, bits) = match table {
                Some(t) => crate::huff::read_bits_with(rest, t, lit_total)?,
                None => crate::huff::read_bits(rest, lens.ok_or(Corrupt)?, lit_total)?,
            };
            Ok((alloc::borrow::Cow::Owned(out), bits.div_ceil(8) as usize))
        }
        _ => Err(Corrupt),
    }
}

/// Nibble plus 255-continuation extension.
fn read_len(payload: &[u8], p: &mut usize, nibble: usize) -> Result<usize, Corrupt> {
    if nibble < 15 {
        return Ok(nibble);
    }
    let mut total = 15usize;
    loop {
        let b = *payload.get(*p).ok_or(Corrupt)?;
        *p += 1;
        total = total.checked_add(usize::from(b)).ok_or(Corrupt)?;
        if b != 255 {
            return Ok(total);
        }
    }
}

/// Copy `len` bytes from `dist` back in the virtual history
/// (`dict ++ out`) onto the end of `out`. Bounds were checked by the
/// caller; this splits into the three copy shapes.
fn copy_match(dict: &[u8], out: &mut Vec<u8>, dist: usize, len: usize) {
    if dist <= out.len() {
        let start = out.len() - dist;
        if dist >= len {
            // Non-overlapping, entirely inside the output: one bulk copy.
            out.extend_from_within(start..start + len);
        } else {
            // Overlapping: a repeating pattern by definition. Double
            // the available run — always whole periods, so the final
            // partial copy still lands on pattern phase.
            let mut remaining = len;
            loop {
                let avail = out.len() - start;
                if avail >= remaining {
                    out.extend_from_within(start..start + remaining);
                    break;
                }
                out.extend_from_within(start..start + avail);
                remaining -= avail;
            }
        }
    } else {
        // Starts in the dictionary; may run across into the output.
        let dstart = dict.len() - (dist - out.len());
        let in_dict = (dict.len() - dstart).min(len);
        out.extend_from_slice(&dict[dstart..dstart + in_dict]);
        let rest = len - in_dict;
        if rest > 0 {
            // Continues at the first produced byte — same virtual run.
            copy_match(dict, out, dist, rest);
        }
    }
}

/// Decode a high-level payload: `[varint lit_total][flag]
/// [literal block][sequence stream]`. Literals come back as one bulk
/// pass (Huffman table loop or a plain slice), then the byte-aligned
/// sequence stream interleaves them with matches — same token grammar
/// as the fast level minus the inline literals.
pub(crate) fn lz_high(
    dict: &[u8],
    lens: Option<&[u8; 256]>,
    table: Option<&crate::huff::DecodeTable>,
    payload: &[u8],
    orig_len: usize,
) -> Result<Vec<u8>, Corrupt> {
    let (lit_total, rest) = crate::read_varint(payload)?;
    let (&flag, rest) = rest.split_first().ok_or(Corrupt)?;
    let (lits, seq_start) = read_literal_block(flag, rest, lit_total, lens, table)?;
    let seqs = rest.get(seq_start..).ok_or(Corrupt)?;
    let mut out = Vec::with_capacity(reserve_for(orig_len, payload.len()));
    let (mut p, mut lp) = (0usize, 0usize);
    loop {
        let token = *seqs.get(p).ok_or(Corrupt)?;
        p += 1;
        let lit_len = read_len(seqs, &mut p, (token >> 4) as usize)?;
        let lit_end = lp.checked_add(lit_len).ok_or(Corrupt)?;
        if lit_end > lits.len() || out.len() + lit_len > orig_len {
            return Err(Corrupt);
        }
        out.extend_from_slice(&lits[lp..lit_end]);
        lp = lit_end;
        if p == seqs.len() {
            break;
        }
        let off_bytes: [u8; 2] =
            seqs.get(p..p + 2).ok_or(Corrupt)?.try_into().map_err(|_| Corrupt)?;
        let dist = usize::from(u16::from_le_bytes(off_bytes));
        p += 2;
        let len = read_len(seqs, &mut p, (token & 0x0f) as usize)? + 4;
        if dist == 0 || dist > out.len() + dict.len() || out.len() + len > orig_len {
            return Err(Corrupt);
        }
        copy_match(dict, &mut out, dist, len);
    }
    if out.len() == orig_len && lp == lits.len() { Ok(out) } else { Err(Corrupt) }
}
