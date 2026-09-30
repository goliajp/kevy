use super::*;

fn argv_from(args: &[&[u8]]) -> Argv {
    let mut a = Argv::default();
    for arg in args {
        a.push(arg);
    }
    a
}

#[test]
fn roundtrip_simple_set() {
    let argv = argv_from(&[b"SET", b"foo", b"bar"]);
    let bytes = encode_frame(42, &argv);
    let (DecodedFrame { offset, argv: decoded }, used) = decode_frame(&bytes).expect("decode");
    assert_eq!(offset, 42);
    assert_eq!(decoded, argv);
    assert_eq!(used, bytes.len());
}

#[test]
fn roundtrip_offset_zero_and_max() {
    // Offset is u64 in the API but wire envelope (RESP integer) caps
    // at i64::MAX. See docs/wire.md + encode_frame's doc comment.
    for offset in [0u64, 1, i64::MAX as u64] {
        let argv = argv_from(&[b"PING"]);
        let bytes = encode_frame(offset, &argv);
        let (DecodedFrame { offset: back, .. }, _) = decode_frame(&bytes).expect("decode");
        assert_eq!(back, offset);
    }
}

#[test]
#[cfg(debug_assertions)] // the trip wire is a debug_assert! — release builds skip the panic by design
#[should_panic(expected = "exceeds i64::MAX")]
fn encoding_offset_above_i64_max_panics_in_debug() {
    // Catches accidental over-encoding before the frame goes on the
    // wire. In release builds the assert is gone and the peer would
    // see a BadOffset; we test the debug-build trip wire here.
    let argv = argv_from(&[b"PING"]);
    let _ = encode_frame(u64::MAX, &argv);
}

#[test]
fn roundtrip_argv_with_binary_and_empty_args() {
    let bin: Vec<u8> = (0u8..=255).collect();
    let argv = argv_from(&[b"HSET", b"key", b"field", &bin, b""]);
    let bytes = encode_frame(7, &argv);
    let (DecodedFrame { argv: decoded, .. }, _) = decode_frame(&bytes).expect("decode");
    assert_eq!(decoded.len(), 5);
    assert_eq!(decoded.get(3), Some(bin.as_slice()));
    assert_eq!(decoded.get(4), Some(&b""[..]));
}

#[test]
fn two_concatenated_frames_decode_in_order() {
    let a = encode_frame(1, &argv_from(&[b"SET", b"k", b"a"]));
    let b = encode_frame(2, &argv_from(&[b"DEL", b"k"]));
    let mut buf = a.clone();
    buf.extend_from_slice(&b);

    let (DecodedFrame { offset: off1, argv: argv1 }, used1) = decode_frame(&buf).expect("frame 1");
    assert_eq!(off1, 1);
    assert_eq!(argv1, argv_from(&[b"SET", b"k", b"a"]));
    assert_eq!(used1, a.len());

    let (DecodedFrame { offset: off2, argv: argv2 }, used2) =
        decode_frame(&buf[used1..]).expect("frame 2");
    assert_eq!(off2, 2);
    assert_eq!(argv2, argv_from(&[b"DEL", b"k"]));
    assert_eq!(used1 + used2, buf.len());
}

#[test]
fn offsets_are_strictly_increasing_when_emitted_in_order() {
    let mut bytes = Vec::new();
    for o in 0u64..16 {
        bytes.extend(encode_frame(o, &argv_from(&[b"PING"])));
    }
    let mut pos = 0;
    let mut last: Option<u64> = None;
    while pos < bytes.len() {
        let (DecodedFrame { offset, .. }, used) = decode_frame(&bytes[pos..]).expect("decode");
        if let Some(prev) = last {
            assert!(offset > prev, "offset {offset} not > prev {prev}");
        }
        last = Some(offset);
        pos += used;
    }
    assert_eq!(last, Some(15));
    assert_eq!(pos, bytes.len());
}

#[test]
fn truncated_envelope_is_truncated_not_bad() {
    // Empty.
    assert_eq!(decode_frame(&[]), Err(WireError::Truncated));
    // Just `*` no header end.
    assert_eq!(decode_frame(b"*"), Err(WireError::Truncated));
    // `*2\r\n` then nothing.
    assert_eq!(decode_frame(b"*2\r\n"), Err(WireError::Truncated));
    // Offset start with no CRLF.
    assert_eq!(decode_frame(b"*2\r\n:42"), Err(WireError::Truncated));
    // Header + offset but inner argv missing.
    assert_eq!(decode_frame(b"*2\r\n:42\r\n"), Err(WireError::Truncated));
    // Header + offset + partial inner array.
    assert_eq!(decode_frame(b"*2\r\n:42\r\n*1\r\n$3\r\nfo"), Err(WireError::Truncated));
}

#[test]
fn wrong_envelope_count_rejected() {
    // *1 instead of *2.
    let bad = b"*1\r\n:42\r\n";
    assert!(matches!(decode_frame(bad), Err(WireError::BadEnvelope)));
    // *3 (future-extension shape) is rejected too.
    let bad3 = b"*3\r\n:42\r\n*0\r\n:0\r\n";
    assert!(matches!(decode_frame(bad3), Err(WireError::BadEnvelope)));
}

#[test]
fn non_array_envelope_rejected() {
    // Starts with `:` instead of `*`.
    let bad = b":42\r\n*1\r\n$4\r\nPING\r\n";
    assert!(matches!(decode_frame(bad), Err(WireError::BadEnvelope)));
}

#[test]
fn offset_not_integer_rejected() {
    // Second element is a bulk string, not an integer.
    let bad = b"*2\r\n$2\r\n42\r\n*1\r\n$4\r\nPING\r\n";
    assert!(matches!(decode_frame(bad), Err(WireError::BadOffset)));
}

#[test]
fn negative_offset_rejected_with_value() {
    let bad = b"*2\r\n:-7\r\n*1\r\n$4\r\nPING\r\n";
    match decode_frame(bad) {
        Err(WireError::NegativeOffset(n)) => assert_eq!(n, -7),
        other => panic!("expected NegativeOffset, got {other:?}"),
    }
}

#[test]
fn malformed_inner_payload_surfaces_bad_payload() {
    // Outer envelope + offset OK, inner claims `*1` but follows with
    // an unknown type byte (`!`) — the inner parser rejects.
    let bad = b"*2\r\n:1\r\n*1\r\n!nope\r\n";
    assert!(matches!(decode_frame(bad), Err(WireError::BadPayload(_))));
}

#[test]
fn offset_with_extra_digits_overflow_rejected() {
    // 21 nines — bigger than u64::MAX (20 digits). parse_decimal
    // returns None on the checked-multiply overflow, and parse_signed
    // returns None on top of that, so we see BadOffset.
    let mut bad = b"*2\r\n:".to_vec();
    bad.extend(std::iter::repeat_n(b'9', 21));
    bad.extend_from_slice(b"\r\n*1\r\n$4\r\nPING\r\n");
    assert!(matches!(decode_frame(&bad), Err(WireError::BadOffset)));
}

// Snapshot-wire tests live in `tests/wire_snapshot.rs`
// as an integration test so this file stays under the 500-LOC
// project ceiling. Only public API there.

#[test]
fn encoded_bytes_are_exactly_what_spec_says() {
    // Hand-spell the spec's example so any future refactor that
    // changes byte order trips this test.
    let argv = argv_from(&[b"SET", b"foo", b"bar"]);
    let bytes = encode_frame(99, &argv);
    let expected = b"*2\r\n:99\r\n*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
    assert_eq!(bytes, expected);
}

#[test]
fn only_a_malformed_payload_has_a_source_and_it_is_the_protocol_error() {
    use std::error::Error;
    let e = WireError::BadPayload(ProtocolError::Malformed("bad bulk length"));
    assert_eq!(e.to_string(), "wire inner payload malformed: malformed frame: bad bulk length");
    assert_eq!(
        e.source().map(ToString::to_string).as_deref(),
        Some("malformed frame: bad bulk length")
    );
    for e in [
        WireError::Truncated,
        WireError::BadEnvelope,
        WireError::BadOffset,
        WireError::NegativeOffset(-1),
    ] {
        assert!(e.source().is_none(), "{e} has no source");
    }
}
