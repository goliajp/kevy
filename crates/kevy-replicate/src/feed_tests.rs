use super::*;
use kevy_resp::Argv;

fn argv(parts: &[&[u8]]) -> Argv {
    let mut a = Argv::default();
    for p in parts {
        a.push(p);
    }
    a
}

fn feed_with(n: usize) -> FeedSource {
    let mut f = FeedSource::new(1, ReplicationSource::new(1 << 20));
    for i in 0..n {
        f.source_mut().push_mutation(&argv(&[b"SET", format!("k{i}").as_bytes(), b"v"]));
    }
    f
}

#[test]
fn read_serves_in_order_and_caps_at_max() {
    let f = feed_with(5);
    let frames = f.read(FeedPosition::new(1, 0), 3).unwrap();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].offset, 0);
    assert_eq!(frames[2].offset, 2);
    // caught-up cursor = empty ok
    assert!(f.read(FeedPosition::new(1, 5), 10).unwrap().is_empty());
}

#[test]
fn stale_generation_resyncs_with_tail() {
    let mut f = feed_with(3);
    f.bump_generation();
    let g2 = f.generation();
    f.source_mut().push_mutation(&argv(&[b"SET", b"new", b"v"]));
    match f.read(FeedPosition::new(1, 2), 10) {
        Err(FeedRead::Resync { tail }) => assert_eq!(tail, FeedPosition::new(g2, 1)),
        other => panic!("expected Resync, got {:?}", other.map(|v| v.len())),
    }
    // old-generation frames are gone — new gen serves only its own
    let frames = f.read(FeedPosition::new(g2, 0), 10).unwrap();
    assert_eq!(frames.len(), 1);
}

/// The availgate failover wedge, distilled: generations are
/// identities, not counters. A bump must never land on a
/// PREDICTABLE next value (old+1 is what a peer node's own bump
/// would produce for a DIFFERENT history), and any mismatched
/// cursor — including one "from the future" of a counter's view —
/// resyncs instead of erroring.
#[test]
fn generations_are_random_identities() {
    let mut a = FeedSource::new(1, ReplicationSource::new(1 << 20));
    let mut b = FeedSource::new(1, ReplicationSource::new(1 << 20));
    a.bump_generation();
    b.bump_generation();
    assert_ne!(a.generation(), 0);
    assert_ne!(a.generation(), 1, "old value must not repeat");
    assert_ne!(a.generation(), 2, "a counter's next value is the collision");
    assert_ne!(
        a.generation(),
        b.generation(),
        "two nodes bumping from the same value must diverge"
    );
    // A cursor from a foreign history (any unknown gen) resyncs.
    let g = a.generation();
    match a.read(FeedPosition::new(g.wrapping_add(1), 0), 10) {
        Err(FeedRead::Resync { tail }) => assert_eq!(tail.generation, g),
        other => panic!("expected Resync, got {:?}", other.map(|v| v.len())),
    }
}

#[test]
fn future_cursors_rejected() {
    let f = feed_with(2);
    // Unknown generation (a counter would call gen 3 "the
    // future") → Resync, not Future: identities have no order.
    assert!(matches!(f.read(FeedPosition::new(3, 0), 1), Err(FeedRead::Resync { .. })));
    // Offset ahead within the CURRENT generation → Future.
    assert!(matches!(f.read(FeedPosition::new(1, 99), 1), Err(FeedRead::Future)));
}

#[test]
fn evicted_offset_resyncs() {
    // Tiny budget: pushing enough evicts the front.
    let mut f = FeedSource::new(1, ReplicationSource::new(64));
    for i in 0..50 {
        f.source_mut().push_mutation(&argv(&[b"SET", format!("k{i}").as_bytes(), b"v"]));
    }
    match f.read(FeedPosition::new(1, 0), 10) {
        Err(FeedRead::Resync { tail }) => assert_eq!(tail, FeedPosition::new(1, 50)),
        other => panic!("expected Resync, got {:?}", other.map(|v| v.len())),
    }
}
