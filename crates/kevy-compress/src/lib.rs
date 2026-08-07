//! `kevy-compress` — corpus-aware LZ for the tiered value log.
//!
//! # Why this exists
//!
//! PostgreSQL compresses **per datum** and skips values under ~2 KB
//! entirely; a 400 B row's redundancy lives *across* rows (field
//! names, enum members, shared prefixes) where a per-datum window
//! cannot see it. kevy's vlog is a **corpus** — a contiguous sample of
//! one keyspace — so a dictionary trained on the file reaches the
//! redundancy PG structurally cannot (RFC
//! `2026-07-26-v5-kevy-compress.md` §2; premise measured alive in
//! `bench/FINDING-2026-08-06-k4-premise-corpus-vs-datum.md`: on
//! identical 400 B values a shared dictionary encodes N values as
//! O(dictionary) + N × ~9 B while per-datum pays 89 B each, forever).
//!
//! # The three measured constraints this crate is shaped by
//!
//! - **Decode ≥ ~1 GB/s** (K1): at 100 MiB/s a 4 KiB cold read spends
//!   38 % of its p99 budget in decode; a token + wildcopy design
//!   measured ~8 GB/s in its naive form. Speed is a requirement of the
//!   design, not a later optimisation.
//! - **Never expand** (K2): per-datum zlib on random 400 B values
//!   *grows* them by 11 B. The raw-frame fallback is therefore part of
//!   the format, not an optimisation.
//! - **The dictionary carries K4**: match-finding refinements move
//!   little; dictionary construction decides how much of the corpus
//!   ceiling is captured. The `train` entry point is deliberately a
//!   replaceable policy behind a stable signature.
//!
//! # Format
//!
//! A frame is `[tag: u8][orig_len: LEB128][payload]`.
//!
//! - `TAG_RAW`: payload is the input verbatim. Chosen whenever LZ
//!   would not save a byte — this is what makes K2 structural.
//! - `TAG_LZ` / `TAG_LZ_DICT`: LZ4-style token stream (4-bit literal
//!   and match-length nibbles with 255-continuation bytes, 16-bit
//!   little-endian offsets, minimum match 4). With `TAG_LZ_DICT`
//!   offsets may reach back past the start of output into the
//!   caller-supplied dictionary, which is virtually prepended history.
//!
//! The dictionary is **a parameter, never a dependency**: this crate
//! knows only bytes. Lifecycle (one dictionary per vlog file, seeded
//! across rotation, disposable with the file) belongs to the caller.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;

mod decode;
mod encode;
mod huff;

/// Frame tag: payload is the original bytes verbatim.
pub const TAG_RAW: u8 = 0;
/// Frame tag: LZ token stream, history is the output alone.
pub const TAG_LZ: u8 = 1;
/// Frame tag: LZ token stream, history is `dict ++ output`.
pub const TAG_LZ_DICT: u8 = 2;
/// Frame tag: high (compaction) level — literals Huffman-coded as one
/// block, byte-aligned sequence stream after it.
pub const TAG_LZH: u8 = 3;
/// Frame tag: high level with dictionary history.
pub const TAG_LZH_DICT: u8 = 4;

/// Longest back-reference the 16-bit offset can express, which also
/// bounds how much trailing dictionary is reachable.
pub const MAX_OFFSET: usize = u16::MAX as usize;

/// Magic prefix of a structured dictionary: `[magic][128 B code
/// lengths][content]`. The embedded table is the file-scoped entropy
/// model (one header per FILE instead of per record — the reason the
/// per-record Huffman header could not amortize at 400 B). A
/// dictionary without the magic is plain content bytes; the vlog is
/// disposable, so this format carries no compatibility burden.
const DICT_MAGIC: &[u8; 5] = b"KVCD1";

/// Split a dictionary into its optional entropy table and its content
/// (the match-history bytes).
fn parse_dict(dict: &[u8]) -> (Option<[u8; 256]>, &[u8]) {
    let Some(rest) = dict.strip_prefix(DICT_MAGIC.as_slice()) else {
        return (None, dict);
    };
    let Some((hdr, content)) = rest.split_at_checked(huff::HEADER_LEN) else {
        return (None, dict);
    };
    let mut lens = [0u8; 256];
    for (i, &b) in hdr.iter().enumerate() {
        lens[i * 2] = b & 0x0f;
        lens[i * 2 + 1] = b >> 4;
    }
    if huff::validate_lens(&lens, 1).is_err() {
        return (None, dict);
    }
    (Some(lens), content)
}

/// Decode failure: the frame does not decode to exactly what its
/// header promises. Corrupt and truncated frames land here — they are
/// rejected, never mis-decoded (K3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Corrupt;

impl core::fmt::Display for Corrupt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("kevy-compress: corrupt or truncated frame")
    }
}

/// Encode `input` into a frame, using `dict` as shared history when it
/// pays. The result is **never longer than `input` plus the frame
/// header** (K2): when LZ cannot save a byte — incompressible input,
/// adversarial input, anything — the frame stores the bytes raw.
#[must_use]
pub fn encode(dict: &[u8], input: &[u8]) -> Vec<u8> {
    let (_, content) = parse_dict(dict);
    let mut frame = Vec::with_capacity(input.len() + MAX_HEADER);
    let (tag, ok) = if input.len() >= encode::MIN_INPUT {
        encode::try_lz(content, input, &mut frame)
    } else {
        (TAG_RAW, false)
    };
    if ok {
        finish_header(&mut frame, tag, input.len());
    } else {
        frame.clear();
        push_header(&mut frame, TAG_RAW, input.len());
        frame.extend_from_slice(input);
    }
    frame
}

/// Encode at the compaction level: same match finder, literals pulled
/// into one Huffman-coded block (zstd's shape). Strictly
/// smallest-wins: the result is the smaller of high / fast / raw, so
/// K2 holds here exactly as it does for [`encode`]. Costs roughly a
/// second serialization pass at encode time — which is the point: a
/// value re-encoded by compaction has proven cold (RFC §3).
#[must_use]
pub fn encode_high(dict: &[u8], input: &[u8]) -> Vec<u8> {
    let (lens, content) = parse_dict(dict);
    let mut frame = Vec::with_capacity(input.len() + MAX_HEADER);
    let (tag, ok) = if input.len() >= encode::MIN_INPUT {
        encode::try_high(content, lens.as_ref(), input, &mut frame)
    } else {
        (TAG_RAW, false)
    };
    if ok {
        finish_header(&mut frame, tag, input.len());
    } else {
        frame.clear();
        push_header(&mut frame, TAG_RAW, input.len());
        frame.extend_from_slice(input);
    }
    frame
}

/// Decode one frame produced by [`encode`]. `dict` must be the same
/// bytes the encoder was given — the tag records whether the frame
/// depends on it at all.
pub fn decode(dict: &[u8], frame: &[u8]) -> Result<Vec<u8>, Corrupt> {
    let (lens, content) = parse_dict(dict);
    let (&tag, rest) = frame.split_first().ok_or(Corrupt)?;
    let (orig_len, payload) = read_varint(rest)?;
    match tag {
        TAG_RAW => {
            if payload.len() != orig_len {
                return Err(Corrupt);
            }
            Ok(payload.to_vec())
        }
        TAG_LZ => decode::lz(&[], payload, orig_len),
        TAG_LZ_DICT => {
            if content.is_empty() {
                return Err(Corrupt);
            }
            decode::lz(content, payload, orig_len)
        }
        TAG_LZH => decode::lz_high(&[], None, payload, orig_len),
        TAG_LZH_DICT => {
            if content.is_empty() {
                return Err(Corrupt);
            }
            decode::lz_high(content, lens.as_ref(), payload, orig_len)
        }
        _ => Err(Corrupt),
    }
}

/// Build a dictionary from sample values under a byte budget.
///
/// v1 policy: evenly-strided whole samples until the budget fills —
/// the simplest construction that makes identical and near-identical
/// corpora hit the "O(dictionary) + N × small" shape. The K4 premise
/// measurement says construction (not match-finding) is where corpus
/// capture is won, so expect this policy to be replaced behind the
/// same signature; its output is bytes, and bytes carry no versioning
/// burden (the dictionary dies with its vlog file).
#[must_use]
pub fn train(samples: &[&[u8]], budget: usize) -> Vec<u8> {
    let mut dict = Vec::with_capacity(budget.min(MAX_OFFSET + 256));
    if samples.is_empty() || budget == 0 {
        return dict;
    }
    // File-scoped entropy table: one 128 B header per FILE amortizes
    // where the per-record header measured itself out at 400 B.
    // Add-one smoothing keeps every byte codable, so a block never
    // falls back merely because it contains a byte the samples lacked.
    // The header spends budget like anything else; a budget too small
    // to leave meaningful content after it skips the table.
    let header = DICT_MAGIC.len() + huff::HEADER_LEN;
    if budget >= header * 8 {
        emit_table(samples, &mut dict);
    }
    // Content is what back-references reach: clamp IT to the offset
    // limit; the header rides above that reach.
    let budget = (budget - dict.len()).min(MAX_OFFSET);
    // Stride so picks span the corpus rather than clustering at the
    // front — rotation seeding hands us *old* files' bytes first, and
    // the tail is as representative as the head. Exact duplicates are
    // skipped: a dictionary is a model, and a second copy of a sample
    // teaches it nothing while charging every record its amortized
    // bytes (the K4-corpora run measured the un-deduped version paying
    // 65 B/value of dictionary for an identical corpus whose whole
    // model is one 400 B value).
    let mut need = budget;
    let avg = samples.iter().map(|s| s.len()).sum::<usize>() / samples.len().max(1);
    let stride = if avg == 0 { 1 } else { (samples.len() * avg / budget).max(1) };
    let mut seen = alloc::collections::BTreeSet::new();
    let mut i = 0;
    while i < samples.len() && need > 0 {
        if seen.insert(fnv1a(samples[i])) {
            let take = samples[i].len().min(need);
            dict.extend_from_slice(&samples[i][..take]);
            need -= take;
        }
        i += stride;
    }
    dict
}

/// The dictionary's file-scoped entropy table: magic + packed code
/// lengths from an add-one-smoothed histogram of the sample bytes.
fn emit_table(samples: &[&[u8]], dict: &mut Vec<u8>) {
    let mut hist = [1u64; 256];
    for s in samples {
        for &b in *s {
            hist[b as usize] += 1;
        }
    }
    dict.extend_from_slice(DICT_MAGIC);
    let lens = huff::code_lengths(&hist);
    for pair in lens.chunks_exact(2) {
        dict.push(pair[0] | (pair[1] << 4));
    }
}

/// Sample identity for training dedup — FNV-1a over the whole sample.
/// A collision merely drops one sample from the dictionary; it cannot
/// affect correctness (the dictionary is advisory bytes).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Largest header: tag byte plus a five-byte LEB128 length.
const MAX_HEADER: usize = 6;

fn push_header(out: &mut Vec<u8>, tag: u8, orig_len: usize) {
    out.push(tag);
    push_varint(out, orig_len);
}

pub(crate) fn push_varint(out: &mut Vec<u8>, value: usize) {
    let mut v = value as u64;
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

/// The LZ path writes its payload first (so it can abort to raw
/// without having written a header); splice the header in front once
/// the payload has proven itself.
fn finish_header(frame: &mut Vec<u8>, tag: u8, orig_len: usize) {
    let mut header = Vec::with_capacity(MAX_HEADER);
    push_header(&mut header, tag, orig_len);
    frame.splice(0..0, header);
}

pub(crate) fn read_varint(buf: &[u8]) -> Result<(usize, &[u8]), Corrupt> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if shift >= 35 {
            return Err(Corrupt);
        }
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok((v as usize, &buf[i + 1..]));
        }
        shift += 7;
    }
    Err(Corrupt)
}

#[cfg(test)]
mod tests;
