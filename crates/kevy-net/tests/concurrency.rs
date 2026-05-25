//! The reactor must multiplex many connections at once. A blocking
//! one-connection-at-a-time server would deadlock this test (later clients
//! never get accepted); the reactor serves all of them.

use kevy_net::{Reactor, Service};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Echoes whatever it receives.
struct Echo;
impl Service for Echo {
    fn on_data(&mut self, input: &mut Vec<u8>, output: &mut Vec<u8>) -> bool {
        output.append(input); // moves all bytes; leaves input empty
        true
    }
}

#[test]
fn many_concurrent_connections() {
    let listener = kevy_sys::tcp_listen([127, 0, 0, 1], 0, 128).unwrap();
    let port = listener.local_port().unwrap();
    let mut reactor = Reactor::new(listener).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let server = std::thread::spawn(move || {
        let mut svc = Echo;
        reactor.run(&mut svc, &stop_thread).unwrap();
    });

    // Open many connections at once, each sending a distinct 4-byte payload.
    const N: usize = 64;
    let mut clients = Vec::with_capacity(N);
    for i in 0..N {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(&(i as u32).to_be_bytes()).unwrap();
        clients.push(s);
    }
    // All of them must echo back correctly — proving simultaneous service.
    for (i, s) in clients.iter_mut().enumerate() {
        let mut got = [0u8; 4];
        s.read_exact(&mut got).unwrap();
        assert_eq!(u32::from_be_bytes(got), i as u32);
    }

    stop.store(true, Ordering::Relaxed);
    drop(clients);
    server.join().unwrap();
}

#[test]
fn close_after_flush_is_honored() {
    // A service that replies once then asks to close.
    struct OneShot;
    impl Service for OneShot {
        fn on_data(&mut self, input: &mut Vec<u8>, output: &mut Vec<u8>) -> bool {
            input.clear();
            output.extend_from_slice(b"bye\n");
            false // close after flushing "bye"
        }
    }

    let listener = kevy_sys::tcp_listen([127, 0, 0, 1], 0, 16).unwrap();
    let port = listener.local_port().unwrap();
    let mut reactor = Reactor::new(listener).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let server = std::thread::spawn(move || {
        let mut svc = OneShot;
        reactor.run(&mut svc, &stop_thread).unwrap();
    });

    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(b"hi").unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap(); // server closes -> read returns EOF
    assert_eq!(buf, b"bye\n");

    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
}
