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

#[test]
fn subscription_is_send_and_sync() {
    // Static-assert via trait bounds: this won't compile if Subscription
    // stops being Send + Sync. Closes the gap mailrs hit on first prod
    // adoption (`Arc<Subscription>` failed because the `mpsc::Receiver`
    // field was !Sync). See type-level doc for the trade-off + memory
    // entry feedback-mailrs-prod-vet-lessons.
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<Subscription>();
}

#[test]
fn arc_subscription_drains_concurrent_recvs_round_robin() {
    // Two threads share one `Arc<Subscription>` and each call `recv` in
    // a loop. The publisher floods 100 frames; the two consumers
    // together must receive exactly 100 frames — no drops, no
    // duplicates. Per the documented single-consumer semantic, each
    // frame goes to exactly one consumer (the receiver mutex
    // serialises concurrent calls and one waiter gets each enqueued
    // frame).
    //
    // **What we DON'T assert**: any specific split between c1 and c2.
    // Under heavy concurrent load (the workspace test binary), one
    // thread can win the lock on every iteration and drain the queue
    // alone — that's still correct single-consumer behaviour. A prior
    // version of this test asserted `c1 > 0 && c2 > 0` and flaked
    // exactly that way under the parallel `cargo test --workspace`
    // scheduler. The Sync property itself is statically asserted by
    // `subscription_is_send_and_sync` above; this test only verifies
    // the runtime contract: no message goes to >1 consumer, none
    // drops on the floor.
    let s = store();
    let sub = std::sync::Arc::new(s.subscribe(&[b"flood"]));

    // Drain the SUBSCRIBE ack so the test only counts publishes.
    let _ack = sub.recv().unwrap();

    let consumer1 = {
        let sub = sub.clone();
        std::thread::spawn(move || {
            let mut count = 0u32;
            while count < 100 {
                match sub.recv_timeout(Duration::from_secs(2)) {
                    Ok(_) => count += 1,
                    Err(_) => break,
                }
            }
            count
        })
    };
    let consumer2 = {
        let sub = sub.clone();
        std::thread::spawn(move || {
            let mut count = 0u32;
            while count < 100 {
                match sub.recv_timeout(Duration::from_secs(2)) {
                    Ok(_) => count += 1,
                    Err(_) => break,
                }
            }
            count
        })
    };

    for i in 0..100u32 {
        let payload = format!("msg-{i:04}");
        let n = s.publish(b"flood", payload.as_bytes());
        assert_eq!(n, 1, "subscriber count was wrong at publish {i}");
    }
    // Give consumers time to drain then close the bus to unblock them.
    std::thread::sleep(Duration::from_millis(100));
    drop(s); // drops the last publishing handle; consumers' recv_timeout will see EOF or drain remaining
    drop(sub); // drop the test's clone so only the two consumer clones remain

    let c1 = consumer1.join().unwrap();
    let c2 = consumer2.join().unwrap();
    assert_eq!(c1 + c2, 100, "got c1={c1}, c2={c2}, expected sum=100");
}

#[test]
fn try_recv_returns_none_under_concurrent_blocking_recv() {
    // Per the type-level doc: try_recv must NOT block on a concurrent
    // blocking recv(). It uses `try_lock` and reports `Ok(None)` on
    // contention. This protects the non-blocking contract for callers
    // who poll try_recv while another thread does the long blocking
    // wait.
    let s = store();
    let sub = std::sync::Arc::new(s.subscribe(&[b"slow"]));
    let _ack = sub.recv().unwrap();

    // Start a blocking recv that will wait a while.
    let blocker = {
        let sub = sub.clone();
        std::thread::spawn(move || {
            let _ = sub.recv_timeout(Duration::from_secs(2));
        })
    };
    // Give the blocker time to acquire the lock.
    std::thread::sleep(Duration::from_millis(50));

    // try_recv must return promptly with Ok(None), not block.
    let start = std::time::Instant::now();
    let res = sub.try_recv().unwrap();
    let elapsed = start.elapsed();
    assert!(res.is_none(), "expected Ok(None) under contention, got {res:?}");
    assert!(
        elapsed < Duration::from_millis(50),
        "try_recv took {elapsed:?} — should not block on receiver mutex"
    );

    // Cleanup: publish so the blocker returns; otherwise it hits the 2s timeout.
    s.publish(b"slow", b"x");
    let _ = blocker.join();
}
