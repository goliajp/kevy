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
