//! `HELLO 3` round-trip — server must (a) accept the protover, (b) flip
//! the conn into RESP3, (c) ship the ack reply already in RESP3 shape
//! (`%7\r\n…`, Map header). Subsequent commands stay valid; **P2 does
//! not yet migrate reply shapes**, so a post-`HELLO 3` SET / GET / etc.
//! still emits RESP2-shape bytes — P3 is where each cmd's reply shape
//! gets RESP3-aware. This test only validates the negotiation.
//!
//! Spec: <https://github.com/antirez/RESP3/blob/master/spec.md>

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(nshards: usize) -> Server {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-hello3-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, nshards, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        let mut ready = false;
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready, "runtime did not come up");
        Server { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Drain bytes from a stream until exactly `n` bytes are read.
fn read_n(s: &mut std::net::TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).unwrap();
    buf
}

#[test]
fn hello_no_arg_keeps_v2_default() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"HELLO"])).unwrap();
    // V2 reply: *14\r\n + 7 (field, value) pairs. First two bytes `*1`
    // suffice to assert RESP2 array shape — the body is server-info.
    let mut head = [0u8; 4];
    c.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"*14\r", "expected `*14\\r\\n` (RESP2 array)");
    // Drain the rest of the array so the conn stays clean.
    let mut sink = vec![0u8; 256];
    let _ = c.read(&mut sink).unwrap();
}

#[test]
fn hello_3_returns_map_shape_and_flips_conn() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"HELLO", b"3"])).unwrap();
    // RESP3 ack: `%7\r\n` (Map header, 7 pairs).
    let mut head = [0u8; 4];
    c.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"%7\r\n", "expected RESP3 Map header `%7\\r\\n`");

    // Drain the 7 pairs (server / version / proto / id / mode / role / modules).
    // Bytes accurate enough to assert: proto field value is `:3\r\n`.
    let mut body = Vec::new();
    let mut chunk = [0u8; 512];
    let n = c.read(&mut chunk).unwrap();
    body.extend_from_slice(&chunk[..n]);
    let needle = b"proto\r\n:3\r\n";
    assert!(
        body.windows(needle.len()).any(|w| w == needle),
        "expected `proto` → `:3` somewhere in HELLO 3 body, got {:?}",
        String::from_utf8_lossy(&body)
    );
}

#[test]
fn hello_2_explicit_returns_array_shape() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"HELLO", b"2"])).unwrap();
    let head = read_n(&mut c, 4);
    assert_eq!(&head, b"*14\r", "HELLO 2 must reply in RESP2 array shape");
    // Drain the rest.
    let mut sink = vec![0u8; 256];
    let _ = c.read(&mut sink).unwrap();
}

#[test]
fn hello_4_unsupported_replies_noproto_error() {
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"HELLO", b"4"])).unwrap();
    // Error reply: `-NOPROTO ...\r\n`.
    let head = read_n(&mut c, 8);
    assert!(
        head.starts_with(b"-NOPROTO"),
        "expected `-NOPROTO` error, got {:?}",
        String::from_utf8_lossy(&head)
    );
    // Drain the rest of the error line.
    let mut tail = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        c.read_exact(&mut byte).unwrap();
        tail.push(byte[0]);
        if tail.ends_with(b"\r\n") {
            break;
        }
    }
}

#[test]
fn hello_3_then_normal_command_still_works() {
    // P2 doesn't migrate reply shapes — a SET / GET after HELLO 3 still
    // gets RESP2-shape replies (P3 will fix). This verifies the conn
    // STAYS USABLE after a HELLO 3 negotiation; it doesn't yet expect
    // RESP3-shape SET / GET replies.
    let srv = Server::start(1);
    let mut c = srv.connect();
    c.write_all(&req(&[b"HELLO", b"3"])).unwrap();
    // Drain the HELLO 3 ack (we already validated its shape elsewhere).
    let mut sink = [0u8; 512];
    let _ = c.read(&mut sink).unwrap();

    c.write_all(&req(&[b"SET", b"hi", b"there"])).unwrap();
    let head = read_n(&mut c, 5);
    assert_eq!(&head, b"+OK\r\n", "SET ack stays +OK (RESP2 shape) in P2");

    c.write_all(&req(&[b"GET", b"hi"])).unwrap();
    let mut buf = vec![0u8; 11]; // `$5\r\nthere\r\n` = 11 bytes
    c.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"$5\r\nthere\r\n");
}

#[test]
fn hello_3_proto_is_per_conn() {
    // Two conns to the same server: one HELLO 3, one HELLO 2.
    // Their replies are independent — proto is a per-conn property.
    let srv = Server::start(1);
    let mut v3 = srv.connect();
    let mut v2 = srv.connect();
    v3.write_all(&req(&[b"HELLO", b"3"])).unwrap();
    v2.write_all(&req(&[b"HELLO", b"2"])).unwrap();

    let v3_head = read_n(&mut v3, 4);
    let v2_head = read_n(&mut v2, 4);
    assert_eq!(&v3_head, b"%7\r\n", "v3 conn should see Map header");
    assert_eq!(&v2_head, b"*14\r", "v2 conn should see Array header");
}
