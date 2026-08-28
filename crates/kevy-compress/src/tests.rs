//! K-criteria at unit scale: round-trip identity, the never-expand
//! guarantee, corrupt-frame rejection, and K4's structural shape.

use alloc::vec::Vec;

use super::*;

fn roundtrip(dict: &[u8], input: &[u8]) -> Vec<u8> {
    let frame = encode(dict, input);
    assert!(
        frame.len() <= input.len() + MAX_HEADER,
        "K2 violated: {} bytes for {} of input",
        frame.len(),
        input.len()
    );
    decode(dict, &frame).expect("own frame must decode")
}

#[test]
fn roundtrips_across_shapes() {
    let cases: &[&[u8]] = &[
        b"",
        b"a",
        b"abcdefg",
        b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        b"the quick brown fox jumps over the lazy dog, twice: \
          the quick brown fox jumps over the lazy dog",
    ];
    for &c in cases {
        assert_eq!(roundtrip(&[], c), c);
    }
    // A long periodic run exercises the overlapping-match doubling.
    let mut periodic = Vec::new();
    for i in 0..10_000u32 {
        periodic.push((i % 7) as u8);
    }
    assert_eq!(roundtrip(&[], &periodic), periodic);
    // Structured text: real compression expected, not just identity.
    let mut text = Vec::new();
    for i in 0..500 {
        text.extend_from_slice(
            alloc::format!("{{\"user\":\"u{i}\",\"role\":\"admin\",\"active\":true}}\n").as_bytes(),
        );
    }
    let frame = encode(&[], &text);
    assert_eq!(decode(&[], &frame).unwrap(), text);
    assert!(frame.len() * 2 < text.len(), "structured text should at least halve");
}

/// K2: pseudo-random (incompressible) input must not expand past the
/// header, and must come back intact.
#[test]
fn k2_incompressible_never_expands() {
    let mut x: u64 = 0x243F_6A88_85A3_08D3;
    let mut random = Vec::with_capacity(4096);
    for _ in 0..4096 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        random.push(x as u8);
    }
    let frame = encode(&[], &random);
    assert!(frame.len() <= random.len() + MAX_HEADER);
    assert_eq!(frame[0], TAG_RAW, "random input should fall back to raw");
    assert_eq!(decode(&[], &frame).unwrap(), random);
}

/// K3: truncations and bit flips are rejected, never mis-decoded into
/// a wrong-but-plausible value.
#[test]
fn k3_corrupt_frames_reject() {
    let mut text = Vec::new();
    for i in 0..200 {
        text.extend_from_slice(alloc::format!("record-{i}: payload payload payload\n").as_bytes());
    }
    let frame = encode(&[], &text);
    for cut in [0, 1, frame.len() / 2, frame.len() - 1] {
        let out = decode(&[], &frame[..cut]);
        assert!(out.is_err() || out.as_deref() == Ok(&text[..]), "truncation at {cut} mis-decoded");
    }
    for flip in [0usize, 1, 2, frame.len() / 2, frame.len() - 1] {
        let mut bad = frame.clone();
        bad[flip] ^= 0x40;
        let out = decode(&[], &bad);
        assert!(
            out.is_err() || out.as_deref() == Ok(&text[..]),
            "bit flip at {flip} produced a wrong value silently"
        );
    }
    assert_eq!(decode(&[], &[]), Err(Corrupt));
    assert_eq!(decode(&[], &[9, 0]), Err(Corrupt), "unknown tag must reject");
}

/// K4, the structural criterion: N identical values against a shared
/// dictionary encode to O(dictionary) + N x small, where a per-datum
/// encoder pays the full per-copy price forever.
#[test]
fn k4_identical_values_collapse_against_the_dictionary() {
    let value: Vec<u8> = (0..400u32).map(|i| (i * 7 % 251) as u8).collect();
    let dict = train(&[&value], MAX_OFFSET);
    let n = 1000;
    let mut with_dict = 0usize;
    let mut per_datum = 0usize;
    for _ in 0..n {
        let f = encode(&dict, &value);
        assert_eq!(decode(&dict, &f).unwrap(), value);
        with_dict += f.len();
        per_datum += encode(&[], &value).len();
    }
    let per_value = with_dict / n;
    assert!(
        per_value <= 16,
        "K4 shape missed: {per_value} B/value against the dictionary"
    );
    assert!(
        per_datum / n >= value.len() / 2,
        "per-datum baseline unexpectedly captured cross-value redundancy"
    );
}

/// A match that starts in the dictionary and runs across into the
/// produced output — the boundary case the decoder splits by hand.
#[test]
fn dictionary_boundary_crossing_match() {
    let dict = b"prefix-prefix-prefix-".to_vec();
    let input = b"prefix-prefix-prefix-prefix-prefix-tail".to_vec();
    let frame = encode(&dict, &input);
    assert_eq!(decode(&dict, &frame).unwrap(), input);
    // The dict-dependent frame must refuse to decode without its dict.
    if frame[0] == TAG_LZ_DICT {
        assert_eq!(decode(&[], &frame), Err(Corrupt));
    }
}

/// train() respects its budget and the offset reach.
#[test]
fn train_bounds_its_budget() {
    let samples: Vec<Vec<u8>> = (0..100).map(|i| alloc::vec![i as u8; 1000]).collect();
    let refs: Vec<&[u8]> = samples.iter().map(|s| s.as_slice()).collect();
    let dict = train(&refs, 32 * 1024);
    assert!(dict.len() <= 32 * 1024);
    let huge = train(&refs, usize::MAX);
    // Content past offset reach is dead weight; the 133-byte container
    // header (magic + entropy table) rides above the reach and is the
    // one exception.
    assert!(huge.len() <= MAX_OFFSET + 133, "content past offset reach is dead weight");
}

/// The compaction level: identity on every shape the fast level
/// covers, strictly-smallest against fast and raw, and rejection on
/// corruption — the same K2/K3 posture one level up.
#[test]
fn high_level_roundtrips_and_never_loses_to_fast() {
    let mut text = Vec::new();
    for i in 0..500 {
        text.extend_from_slice(
            alloc::format!("{{\"user\":\"u{i}\",\"role\":\"admin\",\"active\":true}}\n").as_bytes(),
        );
    }
    let value: Vec<u8> = (0..400u32).map(|i| (i * 7 % 251) as u8).collect();
    let dict = train(&[&value], MAX_OFFSET);
    let mut x: u64 = 0x1234_5678_9abc_def1;
    let mut random = Vec::with_capacity(2048);
    for _ in 0..2048 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        random.push(x as u8);
    }
    for (d, v) in [
        (&[][..], &text),
        (&[][..], &random),
        (dict.as_slice(), &value),
        (&[][..], &alloc::vec![0u8; 10_000]),
    ] {
        let high = encode_high(d, v);
        assert_eq!(&decode(d, &high).unwrap(), v, "high round-trip");
        let fast = encode(d, v);
        assert!(
            high.len() <= fast.len(),
            "high must never lose to fast ({} > {})",
            high.len(),
            fast.len()
        );
        assert!(high.len() <= v.len() + MAX_HEADER, "K2 at the high level");
    }
    // Text actually gains from the entropy layer, not just ties.
    let high = encode_high(&[], &text);
    let fast = encode(&[], &text);
    assert!(
        high.len() < fast.len() * 95 / 100,
        "entropy coding should beat fast by >5% on JSON text ({} vs {})",
        high.len(),
        fast.len()
    );
    // Corruption still rejects, never mis-decodes.
    for flip in [1usize, 8, high.len() / 2, high.len() - 1] {
        let mut bad = high.clone();
        bad[flip] ^= 0x20;
        let out = decode(&[], &bad);
        assert!(
            out.is_err() || out.as_deref() == Ok(&text[..]),
            "bit flip at {flip} mis-decoded silently"
        );
    }
}

/// Regression: fuzz vlog_churn crash 6b733e74 (2026-08-12). A high
/// frame whose ONLY dict dependency was the shared Huffman table
/// (flag-2 literal block, no LZ match into dict content) was tagged
/// TAG_LZH — so decode ran with lens=None and rejected the encoder's
/// own output. The dependency tag must cover the shared table too.
#[test]
fn high_frame_with_shared_table_only_dependency_roundtrips() {
    // A dict whose byte distribution makes the shared table win for
    // the payload's literals, while the payload itself is too distant
    // for any LZ match into dict content.
    let samples: Vec<Vec<u8>> = (0..40).map(|i| vec![0x03u8.wrapping_add(i % 2); 400]).collect();
    let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
    let dict = train(&refs, MAX_OFFSET);
    // Alternating bytes: no 4-byte LZ match into the dict's uniform
    // runs, but both bytes are cheap in its shared table — flag 2
    // wins with used_dict=false, the exact mistagged shape.
    let payload: Vec<u8> = (0..9).map(|i| if i % 2 == 0 { 3u8 } else { 4u8 }).collect();
    let frame = encode_high(&dict, &payload);
    assert_eq!(frame[0], TAG_LZH_DICT, "shared-table use must be tagged as a dict dependency");
    assert_eq!(decode(&dict, &frame).as_deref(), Ok(payload.as_slice()));
}

/// Compat: frames the 5.0.0 encoder wrote with the bug above (flag-2
/// shared-table block under TAG_LZH) are on real disks. decode must
/// recover them by retrying with the dict's lens table instead of
/// declaring corruption — simulated here by rewriting the fixed
/// encoder's TAG_LZH_DICT back to TAG_LZH.
#[test]
fn legacy_mistagged_shared_table_frame_still_decodes() {
    let samples: Vec<Vec<u8>> = (0..40).map(|i| vec![0x03u8.wrapping_add(i % 2); 400]).collect();
    let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
    let dict = train(&refs, MAX_OFFSET);
    let payload: Vec<u8> = (0..9).map(|i| if i % 2 == 0 { 3u8 } else { 4u8 }).collect();
    let mut frame = encode_high(&dict, &payload);
    assert_eq!(frame[0], TAG_LZH_DICT, "setup must produce a shared-table frame");
    frame[0] = TAG_LZH; // what the 5.0.0 encoder emitted
    assert_eq!(decode(&dict, &frame).as_deref(), Ok(payload.as_slice()));
}

/// The two defects the first run of `decode_arbitrary` found.
///
/// That target had been in the tree, unrun, because the fuzz-smoke matrix
/// is hand-written and never grew to include it. Its own doc says it
/// "exists to let the fuzzer hunt for the arithmetic case the checks
/// missed"; it found two inside two minutes. Both inputs are reproduced
/// here byte for byte from `fuzz/artifacts/decode_arbitrary/`.
mod what_the_fuzzer_found {
    /// The fuzz target's own split: first byte picks the dict/frame cut.
    fn feed(data: &[u8]) -> Result<Vec<u8>, crate::Corrupt> {
        let (&split, rest) = data.split_first().expect("non-empty");
        let cut = (split as usize * rest.len()) / 255;
        let (dict, frame) = rest.split_at(cut);
        crate::decode(dict, frame)
    }

    /// `oom-d16de1c6…`: twelve bytes, `malloc(2864709630)`.
    ///
    /// The frame's declared original length went straight to
    /// `Vec::with_capacity`. The decode loop was bounds-checked at every
    /// step — the allocation happened before the loop.
    #[test]
    fn a_declared_length_is_a_claim_not_a_size() {
        let input = [0x01, 0x01, 0xfe, 0xff, 0xff, 0xd5, 0x0a, 0x00, 0x38, 0x00, 0x0a, 0x00];
        assert!(feed(&input).is_err(), "twelve bytes cannot decode to 2.8 GB");

        // That assertion alone does NOT see this defect, and it is worth
        // saying why: the decode returned Err before the fix too — every
        // step of the loop was already bounds-checked. What was wrong was
        // how much got reserved on the way there, and on a platform that
        // overcommits, reserving 2.8 GB of address space fails nothing.
        // Verified: with the clamp removed, the line above still passes.
        // So the bound itself is what gets asserted.
        assert_eq!(
            crate::decode::reserve_for(2_864_709_630, 11),
            11 * 256 + 1024,
            "a claim far past what the frame could produce reserves the ceiling"
        );
        assert_eq!(
            crate::decode::reserve_for(300_000, 200_000),
            300_000,
            "an honest frame reserves exactly what it declares"
        );

        // And the format's expansion ceiling is not a guess: `read_len`
        // adds at most 255 per continuation byte, so 256 is an
        // over-estimate and no real frame is ever short-reserved.
        let big = vec![7u8; 300_000];
        let framed = crate::encode(&[], &big);
        assert_eq!(crate::decode(&[], &framed).unwrap(), big, "honest frames unchanged");
    }

    /// `crash-80b8440d…`: `attempt to subtract with overflow`, huff.rs.
    ///
    /// A header nibble carries 0..=15 and MAX_LEN is 12, so 13, 14 and 15
    /// reached `MAX_LEN - l` inside the Kraft sum — the check whose whole
    /// job is to reject an over-full code space.
    #[test]
    fn a_code_length_past_max_len_is_refused_before_the_kraft_sum() {
        let mut input = vec![0x00, 0x03, 0x19, 0xff, 0xf8, 0x01, 0x01, 0x0a];
        input.extend(std::iter::repeat_n(0xff, 131));
        input.extend([0x23, 0x02, 0x31, 0x02]);
        assert_eq!(input.len(), 143, "the artifact is 143 bytes");
        assert!(feed(&input).is_err(), "an unexpressible code length is corrupt, not a panic");

        let mut lens = [0u8; 256];
        lens[0] = 15;
        assert!(crate::huff::validate_lens(&lens, 1).is_err(), "15 > MAX_LEN");
        lens[0] = 12;
        assert!(crate::huff::validate_lens(&lens, 1).is_ok(), "MAX_LEN itself is expressible");
    }
}

/// The third site, found by CI running what had never run.
///
/// `decode.rs` had two allocations sized by a frame-declared length and both
/// were bounded. `huff::read_bits` had a third, reached through the literal
/// block's `lit_total`, and CI's first run of `decode_arbitrary` asked for
/// 10.9 GB there. Bounding where the fuzzer points is not the same as
/// bounding every place a length out of the frame reaches an allocation —
/// so this asserts the rule, not one input.
#[test]
fn a_symbol_count_from_a_frame_cannot_size_an_allocation() {
    use crate::huff::symbols_fit;
    assert_eq!(symbols_fit(3, 1024), 3, "an honest count is used as-is");
    assert_eq!(
        symbols_fit(10_903_093_247, 40),
        40 * 8 + 64,
        "a claim of ten billion symbols over forty bytes reserves the ceiling"
    );
    // One bit per symbol is the floor of a Huffman code, so a stream can
    // always honour 8 * len — the bound never short-reserves an honest frame.
    for len in [0usize, 1, 7, 64, 4096] {
        assert_eq!(symbols_fit(len * 8, len), len * 8, "8*{len} still fits exactly");
    }
}
