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
    assert!(huge.len() <= MAX_OFFSET, "a dictionary past offset reach is dead weight");
}
