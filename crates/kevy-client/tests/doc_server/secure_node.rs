// A stand-in for a one-node encrypted cluster port, for the rustdoc
// examples: every connection runs the Noise handshake a kevy encrypted port
// runs, then `CLUSTER SLOTS` names this node as the owner of every slot and
// `PING` answers `PONG`. Included as items; returns the port and the
// server's public key.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

pub fn secure_node() -> (u16, [u8; 32]) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            std::thread::spawn(move || serve_conn(sock, port));
        }
    });
    (port, kevy_noise::Keypair::from_secret([1; 32]).public())
}

fn serve_conn(mut sock: TcpStream, port: u16) {
    let mut frames = kevy_noise::Frames::default();
    let Some(m1) = next_frame(&mut sock, &mut frames) else { return };
    let (key, eph) =
        (kevy_noise::Keypair::from_secret([1; 32]), kevy_noise::Keypair::from_secret([2; 32]));
    let Ok((_, r)) = kevy_noise::Responder::accept(&key, eph, b"kevy-client\x001", &m1) else {
        return;
    };
    let (m2, t) = r.finish(b"").unwrap();
    sock.write_all(&kevy_noise::frame(&m2).unwrap()).unwrap();
    let (mut tx, mut rx) = t.split();
    let mut buf = Vec::new();
    loop {
        let Ok(Some((argv, used))) = kevy_resp::parse_command(&buf) else {
            let Some(m) = next_frame(&mut sock, &mut frames) else { return };
            buf.extend(rx.open(&m).unwrap());
            continue;
        };
        buf.drain(..used);
        let reply = match argv.iter().next().map(<[u8]>::to_ascii_uppercase).as_deref() {
            Some(b"CLUSTER") => {
                format!("*1\r\n*3\r\n:0\r\n:16383\r\n*2\r\n$9\r\n127.0.0.1\r\n:{port}\r\n")
            }
            Some(b"PING") => "+PONG\r\n".to_string(),
            _ => "-ERR unknown command\r\n".to_string(),
        };
        let sealed = kevy_noise::frame(&tx.seal(reply.as_bytes()).unwrap()).unwrap();
        if sock.write_all(&sealed).is_err() {
            return;
        }
    }
}

// The next whole Noise message; `None` once the client has closed it.
fn next_frame(sock: &mut TcpStream, frames: &mut kevy_noise::Frames) -> Option<Vec<u8>> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(m) = frames.next() {
            return Some(m);
        }
        match sock.read(&mut chunk).unwrap_or(0) {
            0 => return None,
            n => frames.push(&chunk[..n]),
        }
    }
}
