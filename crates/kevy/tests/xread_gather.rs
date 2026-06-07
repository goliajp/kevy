//! Non-blocking multi-stream `XREAD` across shards (cross-shard gather).
//! Before this, `XREAD … STREAMS s1 s2 …` routed to the first stream's shard
//! only and silently dropped streams living on other shards. `nshards = 8`
//! so the streams almost certainly span shards; the gather must return every
//! non-empty stream, in request order, with `COUNT` honoured.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static GATE: Mutex<()> = Mutex::new(());

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn read_n(s: &mut std::net::TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).unwrap();
    buf
}

fn read_line(s: &mut std::net::TcpStream, out: &mut Vec<u8>) {
    loop {
        out.extend_from_slice(&read_n(s, 1));
        if out.ends_with(b"\r\n") {
            break;
        }
    }
}

fn read_len(s: &mut std::net::TcpStream, out: &mut Vec<u8>) -> i64 {
    let start = out.len();
    read_line(s, out);
    std::str::from_utf8(&out[start..out.len() - 2]).unwrap().parse().unwrap()
}

/// Read exactly one complete RESP reply (handles multi-segment arrival).
fn read_reply(s: &mut std::net::TcpStream) -> Vec<u8> {
    let head = read_n(s, 1);
    let mut out = head.clone();
    match head[0] {
        b'+' | b'-' | b':' => read_line(s, &mut out),
        b'$' => {
            let len = read_len(s, &mut out);
            if len >= 0 {
                out.extend_from_slice(&read_n(s, len as usize + 2));
            }
        }
        b'*' => {
            let n = read_len(s, &mut out);
            for _ in 0..n.max(0) {
                out.extend_from_slice(&read_reply(s));
            }
        }
        other => panic!("unknown reply prefix {other:?}"),
    }
    out
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let _g = GATE.lock().unwrap_or_else(|e| e.into_inner());
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = std::env::temp_dir().join(format!("kevy-xrg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let d = dir.clone();
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::new([127, 0, 0, 1], port, 8, kevy::KevyCommands)
                .with_data_dir(d)
                .run(st)
                .unwrap();
        });
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self { port, dir, stop, handle: Some(handle) }
    }
    fn connect(&self) -> std::net::TcpStream {
        // Retry: under CI load (several 8-shard servers in parallel) the
        // first connect can race the listener becoming ready.
        for _ in 0..400 {
            if let Ok(s) = std::net::TcpStream::connect(("127.0.0.1", self.port)) {
                s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                return s;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("could not connect to test server on port {}", self.port);
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

#[test]
fn xread_multistream_crossshard_returns_all_in_order() {
    let srv = Server::start();
    let mut c = srv.connect();
    for (st, val) in [("sa", "va"), ("sb", "vb"), ("sc", "vc")] {
        c.write_all(&req(&[b"XADD", st.as_bytes(), b"1-0", b"f", val.as_bytes()]))
            .unwrap();
        let _ = read_reply(&mut c);
    }
    c.write_all(&req(&[
        b"XREAD", b"STREAMS", b"sa", b"sb", b"sc", b"0", b"0", b"0",
    ]))
    .unwrap();
    let reply = read_reply(&mut c);
    let s = String::from_utf8_lossy(&reply);
    // All three streams present (the gather), and in request order sa<sb<sc.
    assert!(reply.starts_with(b"*3\r\n"), "expected *3 (three streams), got {s:?}");
    let (pa, pb, pc) = (s.find("sa"), s.find("sb"), s.find("sc"));
    assert!(pa.is_some() && pb.is_some() && pc.is_some(), "missing a stream: {s:?}");
    assert!(pa < pb && pb < pc, "streams out of request order: {s:?}");
    for v in ["va", "vb", "vc"] {
        assert!(s.contains(v), "missing value {v}: {s:?}");
    }
}

#[test]
fn xread_multistream_skips_empty_streams() {
    let srv = Server::start();
    let mut c = srv.connect();
    c.write_all(&req(&[b"XADD", b"hasdata", b"1-0", b"f", b"v"])).unwrap();
    let _ = read_reply(&mut c);
    // `empty` stream doesn't exist; XREAD over both must return only `hasdata`.
    c.write_all(&req(&[b"XREAD", b"STREAMS", b"empty", b"hasdata", b"0", b"0"]))
        .unwrap();
    let reply = read_reply(&mut c);
    let s = String::from_utf8_lossy(&reply);
    assert!(reply.starts_with(b"*1\r\n"), "expected one stream, got {s:?}");
    assert!(s.contains("hasdata") && !s.contains("empty"), "{s:?}");
}

#[test]
fn xread_multistream_all_empty_is_nil() {
    let srv = Server::start();
    let mut c = srv.connect();
    c.write_all(&req(&[b"XREAD", b"STREAMS", b"none1", b"none2", b"0", b"0"]))
        .unwrap();
    assert_eq!(read_reply(&mut c), b"*-1\r\n");
}

#[test]
fn xread_multistream_count_is_honoured_per_stream() {
    let srv = Server::start();
    let mut c = srv.connect();
    for id in ["1-0", "2-0", "3-0"] {
        c.write_all(&req(&[b"XADD", b"cs1", id.as_bytes(), b"f", b"v"])).unwrap();
        let _ = read_reply(&mut c);
        c.write_all(&req(&[b"XADD", b"cs2", id.as_bytes(), b"f", b"v"])).unwrap();
        let _ = read_reply(&mut c);
    }
    // COUNT 1 → at most one entry per stream (ids 1-0).
    c.write_all(&req(&[
        b"XREAD", b"COUNT", b"1", b"STREAMS", b"cs1", b"cs2", b"0", b"0",
    ]))
    .unwrap();
    let reply = read_reply(&mut c);
    let s = String::from_utf8_lossy(&reply);
    assert!(reply.starts_with(b"*2\r\n"), "expected two streams, got {s:?}");
    // Only id 1-0 from each (COUNT 1), not 2-0 / 3-0.
    assert!(s.contains("1-0") && !s.contains("2-0") && !s.contains("3-0"), "COUNT not honoured: {s:?}");
}
