// The server the rustdoc examples talk to, included into each one as items:
// a loopback listener answering a handful of commands from an in-memory
// map, so the examples run without a server of their own. `serve` is the
// plaintext port; `serve_secure` runs the Noise responder a kevy encrypted
// client port runs, then the same commands sealed.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

type Keys = Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>;

// The server's key pair on the encrypted port.
const SERVER_SECRET: [u8; 32] = [1; 32];

// A plaintext server; returns its port.
#[allow(dead_code)]
pub fn serve() -> u16 {
    listen(false)
}

// An encrypted server; returns its port and public key.
#[allow(dead_code)]
pub fn serve_secure() -> (u16, [u8; 32]) {
    (listen(true), kevy_noise::Keypair::from_secret(SERVER_SECRET).public())
}

// A public key as the hex a `server_key=` URL parameter takes.
#[allow(dead_code)]
pub fn hex(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn listen(secure: bool) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let keys = Keys::default();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let keys = Arc::clone(&keys);
            std::thread::spawn(move || serve_conn(sock, secure, &keys));
        }
    });
    port
}

fn serve_conn(mut sock: TcpStream, secure: bool, keys: &Keys) {
    let mut frames = kevy_noise::Frames::default();
    let mut noise = None;
    if secure {
        let Some(m1) = next_frame(&mut sock, &mut frames) else { return };
        let key = kevy_noise::Keypair::from_secret(SERVER_SECRET);
        let eph = kevy_noise::Keypair::from_secret([2; 32]);
        // a client that named another server key is dropped, as a server would
        let Ok((_, r)) = kevy_noise::Responder::accept(&key, eph, b"kevy-client\x001", &m1) else {
            return;
        };
        let (m2, t) = r.finish(b"").unwrap();
        sock.write_all(&kevy_noise::frame(&m2).unwrap()).unwrap();
        noise = Some(t.split());
    }
    let mut buf = Vec::new();
    loop {
        let Ok(Some((argv, used))) = kevy_resp::parse_command(&buf) else {
            let more = match &mut noise {
                Some((_, rx)) => next_frame(&mut sock, &mut frames).map(|m| rx.open(&m).unwrap()),
                None => next_bytes(&mut sock),
            };
            let Some(bytes) = more else { return };
            buf.extend_from_slice(&bytes);
            continue;
        };
        buf.drain(..used);
        let argv: Vec<Vec<u8>> = argv.iter().map(<[u8]>::to_vec).collect();
        let reply = answer(&argv, keys);
        let wire = match &mut noise {
            Some((tx, _)) => kevy_noise::frame(&tx.seal(&reply).unwrap()).unwrap(),
            None => reply,
        };
        if sock.write_all(&wire).is_err() {
            return;
        }
    }
}

fn answer(argv: &[Vec<u8>], keys: &Keys) -> Vec<u8> {
    let bulk = |v: &[u8]| [format!("${}\r\n", v.len()).as_bytes(), v, b"\r\n"].concat();
    let mut keys = keys.lock().unwrap();
    match (argv[0].to_ascii_uppercase().as_slice(), &argv[1..]) {
        (b"PING", []) => b"+PONG\r\n".to_vec(),
        (b"PING" | b"ECHO", [msg]) => bulk(msg),
        (b"SELECT", [_]) => b"+OK\r\n".to_vec(),
        (b"SET", [k, v]) => {
            keys.insert(k.clone(), v.clone());
            b"+OK\r\n".to_vec()
        }
        (b"GET", [k]) => keys.get(k).map_or(b"$-1\r\n".to_vec(), |v| bulk(v)),
        (b"INCR", [k]) | (b"INCRBY", [k, _]) => {
            let parse = |v: &[u8]| std::str::from_utf8(v).ok()?.parse::<i64>().ok();
            let by = argv.get(2).map_or(Some(1), |v| parse(v));
            let n = keys.get(k).map_or(Some(0), |v| parse(v)).zip(by);
            let Some(n) = n.map(|(a, b)| a + b) else {
                return b"-ERR value is not an integer or out of range\r\n".to_vec();
            };
            keys.insert(k.clone(), n.to_string().into_bytes());
            format!(":{n}\r\n").into_bytes()
        }
        _ => {
            format!("-ERR unknown command '{}'\r\n", String::from_utf8_lossy(&argv[0])).into_bytes()
        }
    }
}

// The next bytes off the socket; `None` once the client has closed it.
fn next_bytes(sock: &mut TcpStream) -> Option<Vec<u8>> {
    let mut chunk = [0u8; 4096];
    match sock.read(&mut chunk).unwrap_or(0) {
        0 => None,
        n => Some(chunk[..n].to_vec()),
    }
}

// The next whole Noise message; `None` once the client has closed it.
fn next_frame(sock: &mut TcpStream, frames: &mut kevy_noise::Frames) -> Option<Vec<u8>> {
    loop {
        if let Some(m) = frames.next() {
            return Some(m);
        }
        frames.push(&next_bytes(sock)?);
    }
}
