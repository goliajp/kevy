use super::*;
use crate::{Config, Store};

fn store() -> Store {
    Store::open(Config::default().with_ttl_reaper_manual()).unwrap()
}

#[test]
fn publish_to_no_subscribers_returns_zero() {
    let s = store();
    assert_eq!(s.publish(b"chan", b"hi"), 0);
}

#[test]
fn subscribe_ack_then_message_delivered() {
    let s = store();
    let sub = s.subscribe(&[b"news"]);
    // Drain the SUBSCRIBE ack.
    assert_eq!(
        sub.recv().unwrap(),
        PubsubFrame::Subscribe {
            channel: b"news".to_vec(),
            count: 1,
        }
    );
    // Same store handle (or a clone) can publish.
    assert_eq!(s.publish(b"news", b"hello"), 1);
    assert_eq!(
        sub.recv().unwrap(),
        PubsubFrame::Message {
            channel: b"news".to_vec(),
            payload: b"hello".to_vec(),
        }
    );
}

#[test]
fn store_clone_publishes_reach_other_clones_subscribers() {
    let s1 = store();
    let s2 = s1.clone();
    let sub = s1.subscribe(&[b"x"]);
    let _ = sub.recv().unwrap(); // ack
    assert_eq!(s2.publish(b"x", b"v"), 1);
    assert_eq!(
        sub.recv().unwrap(),
        PubsubFrame::Message {
            channel: b"x".to_vec(),
            payload: b"v".to_vec(),
        }
    );
}

#[test]
fn psubscribe_glob_match_delivers_pmessage() {
    let s = store();
    let sub = s.psubscribe(&[b"news.*"]);
    let _ = sub.recv().unwrap(); // psubscribe ack
    assert_eq!(s.publish(b"news.tech", b"breaking"), 1);
    assert_eq!(
        sub.recv().unwrap(),
        PubsubFrame::Pmessage {
            pattern: b"news.*".to_vec(),
            channel: b"news.tech".to_vec(),
            payload: b"breaking".to_vec(),
        }
    );
    // Non-matching publish does not reach the subscriber.
    assert_eq!(s.publish(b"weather", b"sunny"), 0);
    assert!(sub.try_recv().unwrap().is_none());
}

#[test]
fn duplicate_subscribe_does_not_duplicate_delivery() {
    let s = store();
    let mut sub = s.subscribe(&[b"x"]);
    sub.subscribe(&[b"x"]); // second call to same channel: no-op
    // Drain the two acks (one from subscribe(), one from the second call).
    let a1 = sub.recv().unwrap();
    let a2 = sub.recv().unwrap();
    assert!(matches!(a1, PubsubFrame::Subscribe { count: 1, .. }));
    assert!(matches!(a2, PubsubFrame::Subscribe { count: 1, .. }));
    // Single delivery, despite "double subscribe".
    assert_eq!(s.publish(b"x", b"v"), 1);
    let _ = sub.recv().unwrap();
    assert!(sub.try_recv().unwrap().is_none());
}

#[test]
fn unsubscribe_removes_then_no_more_messages() {
    let s = store();
    let mut sub = s.subscribe(&[b"x"]);
    let _ = sub.recv().unwrap();
    sub.unsubscribe(&[b"x"]);
    // Drain the unsubscribe ack.
    assert!(matches!(
        sub.recv().unwrap(),
        PubsubFrame::Unsubscribe {
            channel: Some(_),
            count: 0
        }
    ));
    // Publishes no longer reach us.
    assert_eq!(s.publish(b"x", b"v"), 0);
}

#[test]
fn unsubscribe_all_with_empty_args_drains_every_channel() {
    let s = store();
    let mut sub = s.subscribe(&[b"a", b"b"]);
    let _ = sub.recv().unwrap();
    let _ = sub.recv().unwrap();
    sub.unsubscribe(&[]);
    // Two unsubscribe acks, one per removed channel.
    for _ in 0..2 {
        assert!(matches!(
            sub.recv().unwrap(),
            PubsubFrame::Unsubscribe {
                channel: Some(_),
                ..
            }
        ));
    }
    // Publishes go nowhere now.
    assert_eq!(s.publish(b"a", b"x"), 0);
    assert_eq!(s.publish(b"b", b"x"), 0);
}

#[test]
fn unsubscribe_when_no_subs_held_emits_nil_channel_ack() {
    let s = store();
    let mut sub = s.subscribe(&[]); // empty start
    sub.unsubscribe(&[]);
    assert!(matches!(
        sub.recv().unwrap(),
        PubsubFrame::Unsubscribe {
            channel: None,
            count: 0
        }
    ));
}

#[test]
fn drop_subscriber_unregisters() {
    let s = store();
    let sub = s.subscribe(&[b"x"]);
    let _ = sub.recv().unwrap();
    assert_eq!(s.publish(b"x", b"v"), 1);
    let _ = sub.recv().unwrap();
    drop(sub);
    assert_eq!(s.publish(b"x", b"v"), 0);
}

#[test]
fn recv_timeout_returns_timeout_when_empty() {
    let s = store();
    let sub = s.subscribe(&[b"x"]);
    // Drain the ack first.
    let _ = sub.recv_timeout(Duration::from_millis(100)).unwrap();
    let err = sub
        .recv_timeout(Duration::from_millis(50))
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}
