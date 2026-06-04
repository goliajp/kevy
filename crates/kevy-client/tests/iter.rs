//! `Subscriber::events()` / `Subscriber::messages()` iterator API.
//!
//! kevy stays 0-deps so the wrapper is `std::iter::Iterator`, not
//! `futures::Stream`. Same ergonomics for sync callers, async wrappers
//! via `spawn_blocking` (see `docs/pubsub.md`).

use kevy_client::{Connection, PubsubEvent, Subscriber};
use std::time::Duration;

#[test]
fn events_iter_yields_every_frame_then_stops_on_eof() {
    const URL: &str = "mem://iter-events-eof";
    let mut sub = Subscriber::open(URL, &[b"chan"]).unwrap();
    let mut conn = Connection::open(URL).unwrap();
    let _ = conn.publish(b"chan", b"a").unwrap();
    let _ = conn.publish(b"chan", b"b").unwrap();
    drop(conn);
    sub.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

    // Collect 3 frames synchronously: 1 subscribe ack + 2 messages.
    let mut frames: Vec<PubsubEvent> = Vec::new();
    for ev in sub.events() {
        frames.push(ev.unwrap());
        if frames.len() == 3 {
            break;
        }
    }
    assert!(matches!(
        frames[0],
        PubsubEvent::Subscribe { count: 1, .. }
    ));
    assert_eq!(
        frames[1],
        PubsubEvent::Message {
            channel: b"chan".to_vec(),
            payload: b"a".to_vec(),
        }
    );
    assert_eq!(
        frames[2],
        PubsubEvent::Message {
            channel: b"chan".to_vec(),
            payload: b"b".to_vec(),
        }
    );
}

#[test]
fn messages_iter_skips_acks() {
    // Publish 2 messages, drain via `messages()` — only the message
    // tuples come out; SUBSCRIBE ack is silently consumed.
    const URL: &str = "mem://iter-messages-skip-acks";
    let mut sub = Subscriber::open(URL, &[b"chan"]).unwrap();
    let mut conn = Connection::open(URL).unwrap();
    let _ = conn.publish(b"chan", b"x").unwrap();
    let _ = conn.publish(b"chan", b"y").unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

    let collected: Vec<(Vec<u8>, Vec<u8>)> = sub
        .messages()
        .take(2)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0], (b"chan".to_vec(), b"x".to_vec()));
    assert_eq!(collected[1], (b"chan".to_vec(), b"y".to_vec()));
}

#[test]
fn events_iter_propagates_non_eof_errors() {
    // A read timeout while iterating must surface as Some(Err(_)) rather
    // than terminating — the caller decides whether to keep going.
    const URL: &str = "mem://iter-events-timeout";
    let mut sub = Subscriber::open(URL, &[b"chan"]).unwrap();
    // Drain the ack synchronously before the timeout test.
    let _ = sub.events().next().unwrap().unwrap();
    sub.set_read_timeout(Some(Duration::from_millis(80)))
        .unwrap();

    let mut it = sub.events();
    let first = it.next().expect("iter should not have terminated");
    let err = first.unwrap_err();
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ),
        "unexpected kind: {:?}",
        err.kind()
    );
}

#[test]
fn messages_iter_handles_pmessage() {
    const URL: &str = "mem://iter-messages-pmessage";
    let mut sub = Subscriber::connect(URL).unwrap();
    sub.psubscribe(&[b"news.*"]).unwrap();
    let mut conn = Connection::open(URL).unwrap();
    let _ = conn.publish(b"news.tech", b"hi").unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

    let (channel, payload) = sub.messages().next().unwrap().unwrap();
    assert_eq!(channel, b"news.tech");
    assert_eq!(payload, b"hi");
}
