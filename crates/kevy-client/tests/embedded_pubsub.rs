//! Integration tests for the embedded pub/sub bus.
//!
//! These exercise the pattern a downstream embedding application
//! relies on: one URL string, used by both
//! `Connection::connect` (publisher) and `Subscriber::connect_channels` (consumer),
//! transparently switches between in-process embed (`mem://name`) and
//! TCP server (`kevy://host:port`) without any scheme-branching at the
//! call site.

use kevy_client::KevyError;
use kevy_client::{Connection, PubsubEvent, Subscriber};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

/// The canonical embedding pattern: open one URL → subscribe in thread A,
/// publish in thread B, recv the message in A.
#[test]
fn cross_thread_publish_recv() {
    const URL: &str = "mem://cross-thread";
    let mut sub = Subscriber::connect_channels(URL, &[b"mail.event"]).unwrap();

    // Drain the SUBSCRIBE ack synchronously before letting the publisher
    // thread fire — otherwise the publisher could race ahead of the bus
    // registration. (In Redis-server land, SUBSCRIBE is round-tripped
    // before PUBLISH; the embed bus has the same ordering invariant.)
    let ack = sub.recv().unwrap();
    assert!(matches!(ack, PubsubEvent::Subscribe { count: 1, .. }));

    let barrier = Arc::new(Barrier::new(2));
    let pub_barrier = barrier.clone();
    let publisher = thread::spawn(move || {
        let mut conn = Connection::connect(URL).unwrap();
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
    let mut sub_a = Subscriber::connect_channels("mem://bus-A", &[b"chan"]).unwrap();
    let _ = sub_a.recv().unwrap(); // drain ack
    let mut pub_b = Connection::connect("mem://bus-B").unwrap();
    assert_eq!(pub_b.publish(b"chan", b"x").unwrap(), 0);

    sub_a.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let err = sub_a.recv().unwrap_err();
    assert!(matches!(err, KevyError::TimedOut));
}

/// Pattern subscriptions reach matching channels across the URL.
#[test]
fn psubscribe_glob_reaches_via_same_url() {
    const URL: &str = "mem://glob-bus";
    let mut sub = Subscriber::connect(URL).unwrap();
    sub.psubscribe(&[b"mail.*"]).unwrap();
    let _ = sub.recv().unwrap(); // psubscribe ack

    let mut pubconn = Connection::connect(URL).unwrap();
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
    let mut s1 = Subscriber::connect_channels(URL, &[b"chan"]).unwrap();
    let mut s2 = Subscriber::connect_channels(URL, &[b"chan"]).unwrap();
    let _ = s1.recv().unwrap(); // ack
    let _ = s2.recv().unwrap(); // ack

    let mut pubconn = Connection::connect(URL).unwrap();
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

/// Anonymous `mem://` (no name) has no shared bus — `Subscriber::connect_channels`
/// rejects it with Unsupported (no other producer can reach it).
#[test]
fn anonymous_mem_url_rejected_at_subscriber_open() {
    let err = Subscriber::connect_channels("mem://", &[b"chan"]).unwrap_err();
    assert!(matches!(err, KevyError::Unsupported(_)));
}

/// Dropping every strong handle of a named bus releases its keyspace —
/// a subsequent open with the same URL gets a fresh Store.
#[test]
fn named_bus_recycles_after_all_handles_drop() {
    const URL: &str = "mem://recycle-bus";
    {
        let mut conn = Connection::connect(URL).unwrap();
        conn.set(b"hot", b"yes").unwrap();
        assert_eq!(conn.get(b"hot").unwrap(), Some(b"yes".to_vec()));
    }
    // All handles dropped. A new open sees an empty keyspace.
    let mut conn2 = Connection::connect(URL).unwrap();
    assert_eq!(conn2.get(b"hot").unwrap(), None);
}

/// Downstream-requested convenience: `recv_message` swallows the SUBSCRIBE ack and
/// returns the next real message directly, sparing callers the
/// `loop { match recv() { _ => continue, Message => break } }` boilerplate.
#[test]
fn recv_message_skips_ack_and_returns_payload() {
    const URL: &str = "mem://recv-message-skip-ack";
    let mut sub = Subscriber::connect_channels(URL, &[b"chan"]).unwrap();
    // Publish BEFORE draining the ack so recv_message has to walk
    // past it to find the message.
    let mut conn = Connection::connect(URL).unwrap();
    let n = conn.publish(b"chan", b"hello").unwrap();
    assert_eq!(n, 1);
    sub.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let (channel, payload) = sub.recv_message().unwrap();
    assert_eq!(channel, b"chan");
    assert_eq!(payload, b"hello");
}
