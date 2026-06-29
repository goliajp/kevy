use super::*;

fn make_set_frame(key: &[u8], val_len: usize) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$");
    f.extend_from_slice(key.len().to_string().as_bytes());
    f.extend_from_slice(b"\r\n");
    f.extend_from_slice(key);
    f.extend_from_slice(b"\r\n$");
    f.extend_from_slice(val_len.to_string().as_bytes());
    f.extend_from_slice(b"\r\n");
    f
}

#[test]
fn generic_probe_matches_4k_set_just_header() {
    let frame = make_set_frame(b"key", 4096);
    match probe_generic_bigbulk(&frame) {
        BigArgGenericProbe::Promote {
            total,
            bytes_present,
            body_start_in_tail,
            body_len,
            bare_set_key_range,
        } => {
            assert_eq!(total, frame.len() + 4096 + 2);
            assert_eq!(bytes_present, frame.len());
            assert_eq!(body_start_in_tail, frame.len());
            assert_eq!(body_len, 4096);
            // The key bulk is `$3\r\nkey\r\n`; the 3-byte content sits
            // at offset = `*3\r\n$3\r\nSET\r\n$3\r\n`.len() == 14.
            let key_start = b"*3\r\n$3\r\nSET\r\n$3\r\n".len();
            assert_eq!(bare_set_key_range, Some((key_start, key_start + 3)));
            assert_eq!(&frame[key_start..key_start + 3], b"key");
        }
        _ => panic!("expected Promote"),
    }
}

#[test]
fn generic_probe_matches_64k_set() {
    let frame = make_set_frame(b"k", 65536);
    assert!(matches!(
        probe_generic_bigbulk(&frame),
        BigArgGenericProbe::Promote { .. }
    ));
}

#[test]
fn generic_probe_rejects_small_set_below_threshold() {
    let frame = make_set_frame(b"k", 100);
    assert!(matches!(
        probe_generic_bigbulk(&frame),
        BigArgGenericProbe::NotApplicable
    ));
}

#[test]
fn generic_probe_rejects_threshold_minus_one() {
    let frame = make_set_frame(b"k", BIG_ARG_PROMOTE_THRESHOLD - 1);
    assert!(matches!(
        probe_generic_bigbulk(&frame),
        BigArgGenericProbe::NotApplicable
    ));
}

#[test]
fn generic_probe_matches_threshold_exact() {
    let frame = make_set_frame(b"k", BIG_ARG_PROMOTE_THRESHOLD);
    assert!(matches!(
        probe_generic_bigbulk(&frame),
        BigArgGenericProbe::Promote { .. }
    ));
}

#[test]
fn generic_probe_accepts_lowercase_set() {
    let mut f = Vec::new();
    f.extend_from_slice(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$4096\r\n");
    assert!(matches!(
        probe_generic_bigbulk(&f),
        BigArgGenericProbe::Promote { .. }
    ));
}

// -- additional generic probe tests --

fn make_setex_frame(key: &[u8], ttl: &[u8], val_len: usize) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(b"*4\r\n$5\r\nSETEX\r\n$");
    f.extend_from_slice(key.len().to_string().as_bytes());
    f.extend_from_slice(b"\r\n");
    f.extend_from_slice(key);
    f.extend_from_slice(b"\r\n$");
    f.extend_from_slice(ttl.len().to_string().as_bytes());
    f.extend_from_slice(b"\r\n");
    f.extend_from_slice(ttl);
    f.extend_from_slice(b"\r\n$");
    f.extend_from_slice(val_len.to_string().as_bytes());
    f.extend_from_slice(b"\r\n");
    f
}

fn make_append_frame(key: &[u8], val_len: usize) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(b"*3\r\n$6\r\nAPPEND\r\n$");
    f.extend_from_slice(key.len().to_string().as_bytes());
    f.extend_from_slice(b"\r\n");
    f.extend_from_slice(key);
    f.extend_from_slice(b"\r\n$");
    f.extend_from_slice(val_len.to_string().as_bytes());
    f.extend_from_slice(b"\r\n");
    f
}

fn make_getset_frame(key: &[u8], val_len: usize) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(b"*3\r\n$6\r\nGETSET\r\n$");
    f.extend_from_slice(key.len().to_string().as_bytes());
    f.extend_from_slice(b"\r\n");
    f.extend_from_slice(key);
    f.extend_from_slice(b"\r\n$");
    f.extend_from_slice(val_len.to_string().as_bytes());
    f.extend_from_slice(b"\r\n");
    f
}

#[test]
fn generic_probe_matches_setex_header_only() {
    let frame = make_setex_frame(b"k", b"100", 8192);
    match probe_generic_bigbulk(&frame) {
        BigArgGenericProbe::Promote {
            total,
            bytes_present,
            body_start_in_tail,
            body_len,
            bare_set_key_range,
        } => {
            assert_eq!(total, frame.len() + 8192 + 2);
            assert_eq!(bytes_present, frame.len());
            assert_eq!(body_start_in_tail, frame.len());
            assert_eq!(body_len, 8192);
            // SETEX is not a bare-SET shape.
            assert_eq!(bare_set_key_range, None);
        }
        _ => panic!("expected Promote"),
    }
}

#[test]
fn generic_probe_matches_append_header_only() {
    let frame = make_append_frame(b"k", 16384);
    assert!(matches!(
        probe_generic_bigbulk(&frame),
        BigArgGenericProbe::Promote { .. }
    ));
}

#[test]
fn generic_probe_matches_getset_header_only() {
    let frame = make_getset_frame(b"k", 65536);
    assert!(matches!(
        probe_generic_bigbulk(&frame),
        BigArgGenericProbe::Promote { .. }
    ));
}

#[test]
fn generic_probe_rejects_small_append() {
    let frame = make_append_frame(b"k", 100);
    assert!(matches!(
        probe_generic_bigbulk(&frame),
        BigArgGenericProbe::NotApplicable
    ));
}

#[test]
fn generic_probe_matches_bare_set() {
    // **v1.25 B.5** (post-2026-06-22): plain `SET k <BIG>` (no
    // options, big value as last bulk) IS accepted by the generic
    // probe. The original B.4 bare-SET fast path was retired
    // because its zero-copy Arc adoption bypassed cross-shard
    // routing — the FrameStitch redispatch through `dispatch_batch`
    // is the correctness-preserving path.
    let frame = make_set_frame(b"k", 8192);
    assert!(matches!(
        probe_generic_bigbulk(&frame),
        BigArgGenericProbe::Promote { .. }
    ));
}

#[test]
fn generic_probe_rejects_get_command() {
    let frame = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";
    assert!(matches!(
        probe_generic_bigbulk(frame),
        BigArgGenericProbe::NotApplicable
    ));
}

#[test]
fn generic_probe_rejects_set_with_options_big_value_not_last() {
    // *5 SET k <BIG> EX 10 — big value is bulk #3 of 5, not last.
    // v1.25.x follow-up. Must bail here.
    let mut f = Vec::new();
    f.extend_from_slice(b"*5\r\n$3\r\nSET\r\n$1\r\nk\r\n$8192\r\n");
    // Fill the value body so it's "complete" at this point in the
    // slab; trailing options bulks aren't here yet.
    f.extend_from_slice(&[b'X'; 8192]);
    f.extend_from_slice(b"\r\n$2\r\nEX\r\n");
    // EX value bulk header but value missing — the LAST bulk is
    // incomplete (the `10`), so big-bulk-is-last fires falsely.
    // Reject because the verb is SET (specialised path handles SET).
    assert!(matches!(
        probe_generic_bigbulk(&f),
        BigArgGenericProbe::NotApplicable
    ));
}

#[test]
fn generic_probe_matches_setex_with_partial_body() {
    let mut f = make_setex_frame(b"k", b"100", 16384);
    let header_len = f.len();
    f.extend_from_slice(&[b'Y'; 1000]);
    match probe_generic_bigbulk(&f) {
        BigArgGenericProbe::Promote {
            total,
            bytes_present,
            ..
        } => {
            assert_eq!(total, header_len + 16384 + 2);
            assert_eq!(bytes_present, header_len + 1000);
        }
        _ => panic!("expected Promote"),
    }
}

#[test]
fn generic_probe_matches_psetex() {
    let mut f = Vec::new();
    f.extend_from_slice(b"*4\r\n$6\r\nPSETEX\r\n$1\r\nk\r\n$5\r\n10000\r\n$8192\r\n");
    assert!(matches!(
        probe_generic_bigbulk(&f),
        BigArgGenericProbe::Promote { .. }
    ));
}

#[test]
fn generic_probe_matches_mset_last_big() {
    // *3 MSET k1 <BIG> — argc=3, verb+key+value.
    let mut f = Vec::new();
    f.extend_from_slice(b"*3\r\n$4\r\nMSET\r\n$2\r\nk1\r\n$8192\r\n");
    assert!(matches!(
        probe_generic_bigbulk(&f),
        BigArgGenericProbe::Promote { .. }
    ));
}

#[test]
fn generic_probe_rejects_mset_big_not_last() {
    // *5 MSET k1 <BIG> k2 v2 — big value is bulk #2 of 5, not last.
    let mut f = Vec::new();
    f.extend_from_slice(b"*5\r\n$4\r\nMSET\r\n$2\r\nk1\r\n$8192\r\n");
    f.extend_from_slice(&[b'X'; 8192]);
    f.extend_from_slice(b"\r\n$2\r\nk2\r\n$");
    assert!(matches!(
        probe_generic_bigbulk(&f),
        BigArgGenericProbe::NotApplicable
    ));
}

#[test]
fn generic_probe_rejects_mset_even_argc() {
    // MSET requires odd argc (verb + N pairs). *4 is malformed.
    let mut f = Vec::new();
    f.extend_from_slice(b"*4\r\n$4\r\nMSET\r\n$2\r\nk1\r\n$1\r\nv\r\n$8192\r\n");
    assert!(matches!(
        probe_generic_bigbulk(&f),
        BigArgGenericProbe::NotApplicable
    ));
}

#[test]
fn generic_probe_rejects_truncated_header() {
    let full = make_setex_frame(b"k", b"100", 16384);
    for cut in 0..full.len() {
        assert!(matches!(
            probe_generic_bigbulk(&full[..cut]),
            BigArgGenericProbe::NotApplicable
        ));
    }
}

#[test]
fn generic_probe_rejects_when_all_bulks_complete() {
    // Full frame already in slab — generic probe shouldn't fire
    // (the normal dispatch path can handle it without BigBulk
    // bookkeeping).
    let mut f = make_setex_frame(b"k", b"100", 100);
    f.extend_from_slice(&[b'Z'; 100]);
    f.extend_from_slice(b"\r\n");
    assert!(matches!(
        probe_generic_bigbulk(&f),
        BigArgGenericProbe::NotApplicable
    ));
}
