// Stand-ins for a primary's replication listener, included by the crate's
// doc examples so each one can run against a real socket without a server.

/// Accept one replica, read its handshake, answer `ack`, write `stream`,
/// then half-close. The handle yields every byte the replica sent
/// (handshake first) once the replica closes its end.
#[allow(dead_code)]
pub fn fake_primary(
    ack: &'static [u8],
    stream: Vec<u8>,
) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<u8>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let primary = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut got = Vec::new();
        let mut chunk = [0u8; 512];
        // read the whole handshake first: closing on unread bytes resets
        // the link instead of ending it
        while kevy_resp::parse_command_into(&got, &mut kevy_resp::Argv::default())
            .expect("handshake is RESP")
            .is_none()
        {
            let n = sock.read(&mut chunk).expect("read handshake");
            got.extend_from_slice(&chunk[..n]);
        }
        sock.write_all(ack).expect("write ack");
        sock.write_all(&stream).expect("write stream");
        sock.shutdown(std::net::Shutdown::Write).expect("half-close");
        // a replica that stops reading early resets the link; what it sent
        // before that is still in `got`
        let _ = sock.read_to_end(&mut got);
        got
    });
    (addr, primary)
}

/// `words` as an argv, the shape a dispatcher hands the replication source.
#[allow(dead_code)]
pub fn argv(words: &[&str]) -> kevy_resp::Argv {
    kevy_resp::Argv::from(words.iter().map(|w| w.as_bytes().to_vec()).collect::<Vec<_>>())
}
