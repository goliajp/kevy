//! End-to-end v0.1 detection: a client speaking real RESP over a TCP socket
//! gets correct PING/ECHO replies through our own socket layer. Hermetic — no
//! external redis-cli required.

use std::io::{Read, Write};

/// Connect, run one round-trip closure, return the server thread for joining.
fn with_server<F: FnOnce(&mut std::net::TcpStream)>(body: F) {
    let listener = kevy_sys::tcp_listen([127, 0, 0, 1], 0, 16).unwrap();
    let port = listener.local_port().unwrap();

    let server = std::thread::spawn(move || {
        let conn = listener.accept().unwrap();
        let mut store = kevy::KeyspaceStore::new();
        kevy::handle_conn(&conn, &mut store).unwrap();
    });

    let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    body(&mut client);
    drop(client); // EOF → handle_conn returns
    server.join().unwrap();
}

/// Read exactly `expected.len()` bytes and assert they match.
fn expect(stream: &mut std::net::TcpStream, expected: &[u8]) {
    let mut buf = vec![0u8; expected.len()];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(buf, expected);
}

#[test]
fn ping_and_echo_over_resp() {
    with_server(|c| {
        c.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
        expect(c, b"+PONG\r\n");

        c.write_all(b"*2\r\n$4\r\nECHO\r\n$5\r\nhello\r\n").unwrap();
        expect(c, b"$5\r\nhello\r\n");

        // PING with an argument echoes it back as a bulk string.
        c.write_all(b"*2\r\n$4\r\nPING\r\n$3\r\nhey\r\n").unwrap();
        expect(c, b"$3\r\nhey\r\n");
    });
}

#[test]
fn inline_command_works() {
    with_server(|c| {
        c.write_all(b"PING\r\n").unwrap();
        expect(c, b"+PONG\r\n");
    });
}

#[test]
fn unknown_command_errors() {
    with_server(|c| {
        c.write_all(b"*1\r\n$7\r\nNOSUCH!\r\n").unwrap();
        let mut buf = [0u8; 64];
        let n = c.read(&mut buf).unwrap();
        assert!(buf[..n].starts_with(b"-ERR unknown command"));
    });
}
