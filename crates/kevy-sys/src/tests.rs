use super::*;
use std::io::{Read, Write};

#[test]
fn peer_gone_tracks_the_kernel_not_the_reader() {
    // The whole point of peer_gone is that it asks the kernel, so it sees a
    // dropped peer even when nothing has read the socket. Accept a conn, do
    // not read it, and check both states.
    let listener = tcp_listen([127, 0, 0, 1], 0, 16).unwrap();
    let port = listener.local_port().unwrap();

    let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let server = listener.accept().unwrap();

    // Peer connected, nothing sent → open, nothing pending → not gone.
    assert!(!server.peer_gone(), "a live peer with no pending data is not gone");

    // Peer sends a byte → data queued → still not gone, AND MSG_PEEK must
    // not consume it (a following read still sees the byte). This is also
    // the primitive's one blind spot: unread data ahead of a FIN hides the
    // FIN, so peer_gone only reports gone once the buffer is drained — which
    // is exactly the case at a block serve, where the client sent its BLPOP
    // and nothing more.
    client.write_all(b"x").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!server.peer_gone(), "a peer with unread data is not gone");
    let mut got = [0u8; 1];
    assert_eq!(server.read(&mut got).unwrap(), 1, "peek did not consume the byte");
    assert_eq!(&got, b"x");

    // Peer drops with the buffer drained → FIN reaches the kernel → gone,
    // even though this side issued no read after the FIN.
    drop(client);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(server.peer_gone(), "a peer that sent FIN is gone");
}

#[test]
fn listen_accept_roundtrip() {
    let listener = tcp_listen([127, 0, 0, 1], 0, 16).unwrap();
    let port = listener.local_port().unwrap();
    assert_ne!(port, 0);

    let server = std::thread::spawn(move || {
        let conn = listener.accept().unwrap();
        let mut b = [0u8; 1];
        assert_eq!(conn.read(&mut b).unwrap(), 1);
        conn.write_all(&b).unwrap();
    });

    let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.write_all(b"Z").unwrap();
    let mut got = [0u8; 1];
    assert_eq!(client.read(&mut got).unwrap(), 1);
    assert_eq!(&got, b"Z");

    server.join().unwrap();
}

#[test]
fn poller_signals_listener_readable() {
    let listener = tcp_listen([127, 0, 0, 1], 0, 16).unwrap();
    listener.set_nonblocking().unwrap();
    let port = listener.local_port().unwrap();

    let poller = Poller::new().unwrap();
    poller.add(listener.raw(), true, false).unwrap();

    let _client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();

    let mut events = Vec::new();
    let n = poller.wait(&mut events, Some(2000)).unwrap();
    assert!(n >= 1, "expected a readiness event");
    assert!(events.iter().any(|e| e.fd == listener.raw() && e.readable));

    // Non-blocking accept should now succeed.
    listener.accept().unwrap();
}

#[test]
fn waker_wakes_poller() {
    let w = std::sync::Arc::new(waker().unwrap());
    let poller = Poller::new().unwrap();
    poller.add(w.read_fd(), true, false).unwrap();

    let w2 = w.clone();
    std::thread::spawn(move || w2.wake().unwrap());

    let mut events = Vec::new();
    let n = poller.wait(&mut events, Some(2000)).unwrap();
    assert!(n >= 1, "waker should have woken the poller");
    assert!(events.iter().any(|e| e.fd == w.read_fd() && e.readable));
    w.drain();
}

#[test]
fn reuseport_allows_shared_port() {
    let l1 = tcp_listen_reuseport([127, 0, 0, 1], 0, 16).unwrap();
    let port = l1.local_port().unwrap();
    // A second listener on the SAME port succeeds only because of SO_REUSEPORT.
    let l2 = tcp_listen_reuseport([127, 0, 0, 1], port, 16).unwrap();
    assert_eq!(l2.local_port().unwrap(), port);
}
