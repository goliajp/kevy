use super::*;

// ----- URL routing -----

#[test]
fn anonymous_mem_rejected_for_subscriber() {
    let err = Subscriber::connect("mem://").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}

#[test]
fn named_mem_resolves_to_embedded() {
    // Just connect, don't subscribe — proves the URL parses + registry
    // hits the embedded branch. Drop immediately afterwards.
    let _sub = Subscriber::connect("mem://named-bus-test").unwrap();
}

#[test]
fn open_with_empty_channels_rejected() {
    let err = Subscriber::open("kevy://127.0.0.1:1", &[]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

// ----- classify (RESP wire path) -----

#[test]
fn classify_subscribe_ack() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"subscribe".to_vec()),
        Reply::Bulk(b"chan".to_vec()),
        Reply::Int(1),
    ]);
    assert_eq!(
        classify(r).unwrap(),
        PubsubEvent::Subscribe {
            channel: b"chan".to_vec(),
            count: 1,
        }
    );
}

#[test]
fn classify_message_event() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"message".to_vec()),
        Reply::Bulk(b"news".to_vec()),
        Reply::Bulk(b"hello".to_vec()),
    ]);
    assert_eq!(
        classify(r).unwrap(),
        PubsubEvent::Message {
            channel: b"news".to_vec(),
            payload: b"hello".to_vec(),
        }
    );
}

#[test]
fn classify_pmessage_event() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"pmessage".to_vec()),
        Reply::Bulk(b"news.*".to_vec()),
        Reply::Bulk(b"news.tech".to_vec()),
        Reply::Bulk(b"hi".to_vec()),
    ]);
    assert_eq!(
        classify(r).unwrap(),
        PubsubEvent::Pmessage {
            pattern: b"news.*".to_vec(),
            channel: b"news.tech".to_vec(),
            payload: b"hi".to_vec(),
        }
    );
}

#[test]
fn classify_unsubscribe_with_nil_channel() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"unsubscribe".to_vec()),
        Reply::Nil,
        Reply::Int(0),
    ]);
    assert_eq!(
        classify(r).unwrap(),
        PubsubEvent::Unsubscribe {
            channel: None,
            count: 0,
        }
    );
}

#[test]
fn classify_rejects_unknown_kind() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"bogus".to_vec()),
        Reply::Bulk(b"x".to_vec()),
        Reply::Int(0),
    ]);
    assert_eq!(classify(r).unwrap_err().kind(), io::ErrorKind::InvalidData);
}

#[test]
fn classify_rejects_wrong_arity() {
    let r = Reply::Array(vec![
        Reply::Bulk(b"subscribe".to_vec()),
        Reply::Bulk(b"x".to_vec()),
    ]);
    assert_eq!(classify(r).unwrap_err().kind(), io::ErrorKind::InvalidData);
}

// ----- remote_host_port -----

#[test]
fn remote_host_port_default_6379() {
    let (h, p) = remote_host_port("kevy://example.com").unwrap();
    assert_eq!(h, "example.com");
    assert_eq!(p, 6379);
}

#[test]
fn remote_host_port_explicit() {
    let (h, p) = remote_host_port("redis://example.com:1234/0").unwrap();
    assert_eq!(h, "example.com");
    assert_eq!(p, 1234);
}

#[test]
fn remote_host_port_userinfo_rejected() {
    assert_eq!(
        remote_host_port("kevy://u:p@h:6379").unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
}

// ----- embedded end-to-end -----

#[test]
fn embed_subscribe_then_publish_via_same_url_delivers() {
    use crate::Connection;
    // Both opened with the SAME URL → registry returns the same Store
    // → the publish reaches the subscriber.
    let mut sub = Subscriber::open("mem://embed-e2e-1", &[b"chan"]).unwrap();
    let mut pubconn = Connection::open("mem://embed-e2e-1").unwrap();
    // Drain the SUBSCRIBE ack first.
    let ack = sub.recv().unwrap();
    assert!(matches!(ack, PubsubEvent::Subscribe { .. }));
    // Publish from the second handle.
    assert_eq!(pubconn.publish(b"chan", b"hi").unwrap(), 1);
    let ev = sub.recv().unwrap();
    assert_eq!(
        ev,
        PubsubEvent::Message {
            channel: b"chan".to_vec(),
            payload: b"hi".to_vec(),
        }
    );
}

#[test]
fn embed_different_url_names_are_isolated() {
    use crate::Connection;
    let mut sub = Subscriber::open("mem://embed-iso-A", &[b"chan"]).unwrap();
    let mut pubconn = Connection::open("mem://embed-iso-B").unwrap();
    // Drain ack.
    let _ack = sub.recv().unwrap();
    assert_eq!(pubconn.publish(b"chan", b"hi").unwrap(), 0);
    sub.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let err = sub.recv().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn subscriber_is_send_and_sync() {
    // kevy_embedded::Subscription is now Send + Sync (Mutex-wrapped
    // mpsc receiver/sender — see kevy-embedded 1.x release notes for
    // the trade-off). TcpStream is also Send + Sync. Therefore
    // kevy_client::Subscriber must also be Send + Sync — Arc<Subscriber>
    // works across async tasks / spawn_blocking jobs.
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<Subscriber>();
}
