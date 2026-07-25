use crate::KevyError;
use super::*;

// ----- URL routing -----

#[test]
fn anonymous_mem_rejected_for_subscriber() {
    let err = Subscriber::connect("mem://").unwrap_err();
    assert!(matches!(err, KevyError::Unsupported(_)));
}

#[test]
fn named_mem_resolves_to_embedded() {
    // Just connect, don't subscribe — proves the URL parses + registry
    // hits the embedded branch. Drop immediately afterwards.
    let _sub = Subscriber::connect("mem://named-bus-test").unwrap();
}

#[test]
fn open_with_empty_channels_rejected() {
    let err = Subscriber::connect_channels("kevy://127.0.0.1:1", &[]).unwrap_err();
    assert!(matches!(err, KevyError::InvalidInput(_)));
}

// The RESP wire-frame classifier is canonical in `kevy_resp_client`
// (`classify_pubsub`) and unit-tested there; the sync `recv` path just
// routes through it. Only the embedded-only + URL-routing surface is
// tested locally.

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
    assert!(matches!(remote_host_port("kevy://u:p@h:6379").unwrap_err(), KevyError::Unsupported(_)));
}

// ----- embedded end-to-end -----

#[test]
fn embed_subscribe_then_publish_via_same_url_delivers() {
    use crate::Connection;
    // Both opened with the SAME URL → registry returns the same Store
    // → the publish reaches the subscriber.
    let mut sub = Subscriber::connect_channels("mem://embed-e2e-1", &[b"chan"]).unwrap();
    let mut pubconn = Connection::connect("mem://embed-e2e-1").unwrap();
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
    let mut sub = Subscriber::connect_channels("mem://embed-iso-A", &[b"chan"]).unwrap();
    let mut pubconn = Connection::connect("mem://embed-iso-B").unwrap();
    // Drain ack.
    let _ack = sub.recv().unwrap();
    assert_eq!(pubconn.publish(b"chan", b"hi").unwrap(), 0);
    sub.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let err = sub.recv().unwrap_err();
    assert!(matches!(err, KevyError::TimedOut));
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
