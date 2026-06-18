//! Snapshot wire-format tests (T1.22). Lives as an integration test
//! so the snapshot encoder/decoder coverage doesn't push
//! `src/wire.rs` past the 500-LOC project ceiling. Only exercises
//! the public `wire` API.

use kevy_replicate::wire::{
    SNAPSHOT_CHUNK_MAX, SNAPSHOT_LINE_MAX, SnapshotMarker, WireError, decode_snapshot_chunk,
    decode_snapshot_marker, encode_snapshot_begin, encode_snapshot_chunk, encode_snapshot_end,
};

#[test]
fn snapshot_begin_marker_round_trip() {
    let bytes = encode_snapshot_begin();
    assert_eq!(bytes, b"+SNAPSHOT\r\n");
    let (marker, used) = decode_snapshot_marker(&bytes).unwrap().unwrap();
    assert_eq!(marker, SnapshotMarker::Begin);
    assert_eq!(used, bytes.len());
}

#[test]
fn snapshot_end_marker_round_trip_for_various_offsets() {
    for off in [0u64, 1, 42, u64::from(u32::MAX), i64::MAX as u64] {
        let bytes = encode_snapshot_end(off);
        let (marker, used) = decode_snapshot_marker(&bytes).unwrap().unwrap();
        assert_eq!(marker, SnapshotMarker::End(off), "offset {off}");
        assert_eq!(used, bytes.len());
    }
}

#[test]
fn snapshot_chunk_round_trip_empty_small_and_max() {
    for size in [0usize, 1, 256, 4096, SNAPSHOT_CHUNK_MAX] {
        let data: Vec<u8> = (0..size).map(|i| (i & 0xff) as u8).collect();
        let bytes = encode_snapshot_chunk(&data);
        let (chunk, used) = decode_snapshot_chunk(&bytes).unwrap();
        assert_eq!(chunk, data.as_slice(), "size {size}");
        assert_eq!(used, bytes.len());
    }
}

#[test]
fn snapshot_chunk_with_binary_payload() {
    let data: Vec<u8> = (0..=255u8).chain(0..=255).collect();
    let bytes = encode_snapshot_chunk(&data);
    let (chunk, _) = decode_snapshot_chunk(&bytes).unwrap();
    assert_eq!(chunk, data.as_slice());
}

#[test]
fn snapshot_marker_returns_none_for_non_plus_byte() {
    let r = decode_snapshot_marker(b"*2\r\n:0\r\n*1\r\n$4\r\nPING\r\n");
    assert!(matches!(r, Ok(None)));
    let r = decode_snapshot_marker(b"$5\r\nhello\r\n");
    assert!(matches!(r, Ok(None)));
}

#[test]
fn snapshot_marker_truncated_when_no_crlf_yet() {
    assert_eq!(decode_snapshot_marker(b""), Err(WireError::Truncated));
    assert_eq!(decode_snapshot_marker(b"+SNAP"), Err(WireError::Truncated));
    assert_eq!(
        decode_snapshot_marker(b"+SNAPSHOT_END 42"),
        Err(WireError::Truncated)
    );
}

#[test]
fn snapshot_marker_unknown_simple_string_rejected() {
    let r = decode_snapshot_marker(b"+PONG\r\n");
    assert!(matches!(r, Err(WireError::BadEnvelope)), "got {r:?}");
    let r = decode_snapshot_marker(b"+\r\n");
    assert!(matches!(r, Err(WireError::BadEnvelope)));
    let r = decode_snapshot_marker(b"+SNAPSHOT_END abc\r\n");
    assert!(matches!(r, Err(WireError::BadEnvelope)));
}

#[test]
fn snapshot_marker_line_cap_rejects_oversize() {
    let mut bad = Vec::from(b"+SNAPSHOT".as_slice());
    bad.extend(std::iter::repeat_n(b'X', SNAPSHOT_LINE_MAX + 1));
    let r = decode_snapshot_marker(&bad);
    assert!(matches!(r, Err(WireError::BadEnvelope)), "got {r:?}");
}

#[test]
fn snapshot_chunk_truncated_paths() {
    assert_eq!(decode_snapshot_chunk(b""), Err(WireError::Truncated));
    assert_eq!(decode_snapshot_chunk(b"$10"), Err(WireError::Truncated));
    assert_eq!(
        decode_snapshot_chunk(b"$5\r\nhel"),
        Err(WireError::Truncated)
    );
    assert_eq!(
        decode_snapshot_chunk(b"$5\r\nhello"),
        Err(WireError::Truncated)
    );
}

#[test]
fn snapshot_chunk_oversize_header_rejected() {
    let oversize = SNAPSHOT_CHUNK_MAX + 1;
    let header = format!("${oversize}\r\n");
    let r = decode_snapshot_chunk(header.as_bytes());
    assert!(matches!(r, Err(WireError::BadEnvelope)));
}

#[test]
fn snapshot_chunk_non_dollar_header_rejected() {
    let r = decode_snapshot_chunk(b"!5\r\nhello\r\n");
    assert!(matches!(r, Err(WireError::BadEnvelope)));
}

#[test]
fn snapshot_chunk_non_numeric_length_rejected() {
    let r = decode_snapshot_chunk(b"$abc\r\nhello\r\n");
    assert!(matches!(r, Err(WireError::BadEnvelope)));
}

#[test]
fn snapshot_chunk_wrong_trailing_bytes_rejected() {
    let r = decode_snapshot_chunk(b"$5\r\nhelloXX");
    assert!(matches!(r, Err(WireError::BadEnvelope)));
}

#[test]
fn full_snapshot_stream_decodes_in_order() {
    let mut buf = Vec::new();
    buf.extend(encode_snapshot_begin());
    let payloads: [&[u8]; 3] = [b"chunk-1", b"second-chunk-data", b"last"];
    for p in &payloads {
        buf.extend(encode_snapshot_chunk(p));
    }
    buf.extend(encode_snapshot_end(7777));

    let mut pos = 0;
    let (marker, used) = decode_snapshot_marker(&buf[pos..]).unwrap().unwrap();
    assert_eq!(marker, SnapshotMarker::Begin);
    pos += used;
    for expected in &payloads {
        let (chunk, used) = decode_snapshot_chunk(&buf[pos..]).unwrap();
        assert_eq!(chunk, *expected);
        pos += used;
    }
    let (end_marker, used) = decode_snapshot_marker(&buf[pos..]).unwrap().unwrap();
    assert_eq!(end_marker, SnapshotMarker::End(7777));
    pos += used;
    assert_eq!(pos, buf.len());
}

#[test]
#[cfg(debug_assertions)] // release builds optimise the debug_assert! out
#[should_panic(expected = "snapshot chunk")]
fn encoding_oversize_chunk_panics_in_debug() {
    let data = vec![0u8; SNAPSHOT_CHUNK_MAX + 1];
    let _ = encode_snapshot_chunk(&data);
}
