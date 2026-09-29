use super::*;
use crate::wire::decode_frame;

fn argv(args: &[&[u8]]) -> Argv {
    let mut a = Argv::default();
    for arg in args {
        a.push(arg);
    }
    a
}

#[test]
fn fresh_source_is_empty() {
    let s = ReplicationSource::new(1024);
    assert!(s.is_empty());
    assert_eq!(s.len(), 0);
    assert_eq!(s.next_offset(), 0);
    assert_eq!(s.oldest_offset(), None);
    assert_eq!(s.newest_offset(), None);
    assert_eq!(s.buffered_bytes(), 0);
}

#[test]
fn push_assigns_monotonic_offsets() {
    let mut s = ReplicationSource::new(64 * 1024);
    let o0 = s.push_mutation(&argv(&[b"SET", b"a", b"1"]));
    let o1 = s.push_mutation(&argv(&[b"SET", b"b", b"2"]));
    let o2 = s.push_mutation(&argv(&[b"DEL", b"a"]));
    assert_eq!((o0, o1, o2), (0, 1, 2));
    assert_eq!(s.oldest_offset(), Some(0));
    assert_eq!(s.newest_offset(), Some(2));
    assert_eq!(s.next_offset(), 3);
    assert_eq!(s.len(), 3);
}

#[test]
fn pushed_frames_decode_back_to_the_pushed_argv() {
    let mut s = ReplicationSource::new(1024);
    let a = argv(&[b"HSET", b"h", b"f", b"v"]);
    let off = s.push_mutation(&a);
    let frame = s.buf.front().expect("one frame");
    assert_eq!(frame.offset, off);
    let (crate::replica::DecodedFrame { offset: decoded_off, argv: decoded_argv }, used) =
        decode_frame(&frame.bytes).expect("decode");
    assert_eq!(decoded_off, off);
    assert_eq!(decoded_argv, a);
    assert_eq!(used, frame.bytes.len());
}

#[test]
fn eviction_drops_oldest_when_budget_exceeded() {
    // Each frame encodes to ~37 bytes (envelope + offset + 3-arg SET).
    // Budget of 80 bytes holds 2 frames; pushing a 3rd evicts oldest.
    let mut s = ReplicationSource::new(80);
    let _ = s.push_mutation(&argv(&[b"SET", b"a", b"1"]));
    let _ = s.push_mutation(&argv(&[b"SET", b"b", b"2"]));
    assert_eq!(s.oldest_offset(), Some(0));
    let _ = s.push_mutation(&argv(&[b"SET", b"c", b"3"]));
    assert_eq!(s.oldest_offset(), Some(1));
    assert_eq!(s.newest_offset(), Some(2));
    assert!(s.buffered_bytes() <= 80);
    // next_offset keeps climbing even when older frames are evicted.
    assert_eq!(s.next_offset(), 3);
}

#[test]
fn oversized_single_frame_is_retained_against_budget() {
    // Budget of 8 bytes — smaller than any real frame. The most
    // recent push always survives so a freshly-applied write is
    // never lost before any replica can see it.
    let mut s = ReplicationSource::new(8);
    let off = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
    assert_eq!(s.len(), 1);
    assert_eq!(s.oldest_offset(), Some(off));
    assert!(s.buffered_bytes() > 8); // ran over budget; expected.
    // Pushing again still keeps only the newest (older is evicted).
    let off2 = s.push_mutation(&argv(&[b"DEL", b"k"]));
    assert_eq!(s.len(), 1);
    assert_eq!(s.oldest_offset(), Some(off2));
}

#[test]
fn frames_from_at_exact_offset_returns_that_frame_first() {
    let mut s = ReplicationSource::new(1024);
    for i in 0..5 {
        let _ = s.push_mutation(&argv(&[b"SET", b"k", format!("{i}").as_bytes()]));
    }
    let mut it = s.frames_from(2).unwrap();
    let f = it.next().expect("frame");
    assert_eq!(f.offset, 2);
    let remaining: Vec<u64> = it.map(|f| f.offset).collect();
    assert_eq!(remaining, vec![3, 4]);
}

#[test]
fn frames_from_at_next_offset_is_empty_caught_up() {
    let mut s = ReplicationSource::new(1024);
    let _ = s.push_mutation(&argv(&[b"PING"]));
    let _ = s.push_mutation(&argv(&[b"PING"]));
    let it = s.frames_from(s.next_offset()).unwrap();
    assert_eq!(it.count(), 0);
}

#[test]
fn frames_from_too_old_after_eviction() {
    // Tight budget; push enough to evict offset 0.
    let mut s = ReplicationSource::new(80);
    for _ in 0..5 {
        let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
    }
    // Offset 0 was evicted.
    assert!(s.oldest_offset().unwrap() > 0);
    assert!(matches!(s.frames_from(0), Err(FromOffset::TooOld)));
}

#[test]
fn frames_from_future_offset_rejected() {
    let mut s = ReplicationSource::new(1024);
    let _ = s.push_mutation(&argv(&[b"PING"]));
    // next_offset is 1; asking for 2 is a future-offset peer.
    assert!(matches!(s.frames_from(2), Err(FromOffset::Future)));
}

#[test]
fn frames_from_empty_source_at_zero_is_caught_up_not_too_old() {
    // A fresh source has nothing buffered but is at offset 0; a
    // replica asking from 0 is up-to-date (the source has nothing
    // to send yet), not too-old.
    let s = ReplicationSource::new(1024);
    assert_eq!(s.frames_from(0).unwrap().count(), 0);
    // Asking for offset 1 (one past empty next_offset 0) = Future.
    assert!(matches!(s.frames_from(1), Err(FromOffset::Future)));
}

#[test]
fn push_mutation_accepts_argv_borrowed_from_dispatcher_hot_path() {
    // The reactor's local fast path holds the parsed argv as an
    // `ArgvBorrowed` over the connection read buffer (zero-copy);
    // `push_mutation` must accept that view directly, not force
    // a materialised `Argv`. Parse one with the public parser and
    // push it; the decoded round-trip must match a hand-built Argv
    // of the same command.
    let resp = b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
    let (borrowed, consumed) =
        kevy_resp::parse_command_borrowed(resp).expect("parse ok").expect("complete frame");
    assert_eq!(consumed, resp.len());

    let mut s = ReplicationSource::new(1024);
    let off = s.push_mutation(&borrowed);
    assert_eq!(off, 0);

    let frame = s.buf.front().expect("one frame");
    let (crate::replica::DecodedFrame { offset: decoded_off, argv: decoded_argv }, _) =
        crate::wire::decode_frame(&frame.bytes).expect("decode");
    assert_eq!(decoded_off, 0);
    assert_eq!(decoded_argv, argv(&[b"SET", b"foo", b"bar"]));
}

#[test]
fn buffered_bytes_tracks_actual_frame_total() {
    let mut s = ReplicationSource::new(1024);
    let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
    let _ = s.push_mutation(&argv(&[b"DEL", b"k"]));
    let actual: usize = s.buf.iter().map(|f| f.bytes.len()).sum();
    assert_eq!(s.buffered_bytes(), actual);
}

#[test]
fn drop_up_to_evicts_below_watermark() {
    // drop_up_to(w) evicts every frame with offset < w.
    let mut s = ReplicationSource::new(64 * 1024);
    for i in 0..5 {
        let v = format!("v{i}");
        let _ = s.push_mutation(&argv(&[b"SET", b"k", v.as_bytes()]));
    }
    assert_eq!(s.len(), 5);
    let bytes_before = s.buffered_bytes();
    // Watermark = 3 → drop offsets 0, 1, 2; keep 3, 4.
    s.drop_up_to(3);
    assert_eq!(s.len(), 2);
    assert_eq!(s.oldest_offset(), Some(3));
    assert_eq!(s.newest_offset(), Some(4));
    // bytes accounting must shrink.
    assert!(s.buffered_bytes() < bytes_before);
    // Frames-from at the watermark works without TooOld.
    let kept: Vec<_> = s.frames_from(3).unwrap().collect();
    assert_eq!(kept.len(), 2);
}

#[test]
fn drop_up_to_below_oldest_is_noop() {
    let mut s = ReplicationSource::new(64 * 1024);
    let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
    let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
    assert_eq!(s.oldest_offset(), Some(0));
    s.drop_up_to(0); // already-at-or-past-oldest
    assert_eq!(s.len(), 2);
}

#[test]
fn drop_up_to_at_or_past_newest_drops_everything() {
    let mut s = ReplicationSource::new(64 * 1024);
    for _ in 0..3 {
        let _ = s.push_mutation(&argv(&[b"SET", b"k", b"v"]));
    }
    s.drop_up_to(99);
    assert!(s.is_empty());
    assert_eq!(s.buffered_bytes(), 0);
}
