//! Integration tests for the embedded pub/sub bus (v1.3.0).
//!
//! These exercise the mailrs-style pattern: one URL string, used by both
//! `Connection::open` (publisher) and `Subscriber::open` (consumer),
//! transparently switches between in-process embed (`mem://name`) and
//! TCP server (`kevy://host:port`) without any scheme-branching at the
//! call site.

use kevy_client::{Connection, PubsubEvent, Subscriber};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

/// The canonical mailrs pattern: open one URL → subscribe in thread A,
/// publish in thread B, recv the message in A.
#[test]
fn mailrs_pattern_cross_thread_publish_recv() {
    const URL: &str = "mem://mailrs-cross-thread";
    let mut sub = Subscriber::open(URL, &[b"mail.event"]).unwrap();

    // Drain the SUBSCRIBE ack synchronously before letting the publisher
    // thread fire — otherwise the publisher could race ahead of the bus
    // registration. (In Redis-server land, SUBSCRIBE is round-tripped
    // before PUBLISH; the embed bus has the same ordering invariant.)
    let ack = sub.recv().unwrap();
    assert!(matches!(ack, PubsubEvent::Subscribe { count: 1, .. }));

    let barrier = Arc::new(Barrier::new(2));
    let pub_barrier = barrier.clone();
    let publisher = thread::spawn(move || {
        let mut conn = Connection::open(URL).unwrap();
        pub_barrier.wait();
        conn.publish(b"mail.event", b"recipient=foo@bar.example")
            .unwrap()
    });
    barrier.wait();
    let n = publisher.join().unwrap();
    assert_eq!(n, 1);

    sub.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let ev = sub.recv().unwrap();
    assert_eq!(
        ev,
        PubsubEvent::Message {
            channel: b"mail.event".to_vec(),
            payload: b"recipient=foo@bar.example".to_vec(),
        }
    );
}

/// Two distinct named URLs are independent buses — no cross-talk.
#[test]
fn distinct_named_urls_have_independent_buses() {
    let mut sub_a = Subscriber::open("mem://bus-A", &[b"chan"]).unwrap();
    let _ = sub_a.recv().unwrap(); // drain ack
    let mut pub_b = Connection::open("mem://bus-B").unwrap();
    assert_eq!(pub_b.publish(b"chan", b"x").unwrap(), 0);

    sub_a.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let err = sub_a.recv().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}

/// Pattern subscriptions reach matching channels across the URL.
#[test]
fn psubscribe_glob_reaches_via_same_url() {
    const URL: &str = "mem://glob-bus";
    let mut sub = Subscriber::connect(URL).unwrap();
    sub.psubscribe(&[b"mail.*"]).unwrap();
    let _ = sub.recv().unwrap(); // psubscribe ack

    let mut pubconn = Connection::open(URL).unwrap();
    assert_eq!(pubconn.publish(b"mail.inbound", b"x").unwrap(), 1);
    assert_eq!(pubconn.publish(b"weather", b"sunny").unwrap(), 0);

    let ev = sub.recv().unwrap();
    assert_eq!(
        ev,
        PubsubEvent::Pmessage {
            pattern: b"mail.*".to_vec(),
            channel: b"mail.inbound".to_vec(),
            payload: b"x".to_vec(),
        }
    );
}

/// Multiple subscribers on the same channel all get the message; publish
/// returns the aggregate count.
#[test]
fn fan_out_to_multiple_subscribers() {
    const URL: &str = "mem://fanout-bus";
    let mut s1 = Subscriber::open(URL, &[b"chan"]).unwrap();
    let mut s2 = Subscriber::open(URL, &[b"chan"]).unwrap();
    let _ = s1.recv().unwrap(); // ack
    let _ = s2.recv().unwrap(); // ack

    let mut pubconn = Connection::open(URL).unwrap();
    assert_eq!(pubconn.publish(b"chan", b"hello").unwrap(), 2);

    for sub in [&mut s1, &mut s2] {
        sub.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let ev = sub.recv().unwrap();
        assert_eq!(
            ev,
            PubsubEvent::Message {
                channel: b"chan".to_vec(),
                payload: b"hello".to_vec(),
            }
        );
    }
}

/// Anonymous `mem://` (no name) has no shared bus — `Subscriber::open`
/// rejects it with Unsupported (no other producer can reach it).
#[test]
fn anonymous_mem_url_rejected_at_subscriber_open() {
    let err = Subscriber::open("mem://", &[b"chan"]).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}

/// Dropping every strong handle of a named bus releases its keyspace —
/// a subsequent open with the same URL gets a fresh Store.
#[test]
fn named_bus_recycles_after_all_handles_drop() {
    const URL: &str = "mem://recycle-bus";
    {
        let mut conn = Connection::open(URL).unwrap();
        conn.set(b"hot", b"yes").unwrap();
        assert_eq!(conn.get(b"hot").unwrap(), Some(b"yes".to_vec()));
    }
    // All handles dropped. A new open sees an empty keyspace.
    let mut conn2 = Connection::open(URL).unwrap();
    assert_eq!(conn2.get(b"hot").unwrap(), None);
}
