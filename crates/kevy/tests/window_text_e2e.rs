//! The text window's semantic-equivalence gate: a windowed table's
//! TEXT index freezes its out-of-window documents into cold bucket
//! segments, and bare-term MATCH answers — scores included — must be
//! byte-identical to a never-windowed control over the same rows.
//! Clause-carrying queries refuse by name while cold buckets exist.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port()
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Server {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!("kevy-wintext-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = kevy_config::Config::default();
        cfg.server.port = port;
        cfg.server.threads = 1;
        let state = Arc::new(kevy::RuntimeState::new(Arc::new(cfg), dir.clone(), 1).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::with_state(state))
                .bind([127, 0, 0, 1], port)
                .shards(1)
                .with_data_dir(dir_thread)
                .run(stop_thread)
                .unwrap();
        });
        for _ in 0..2000 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Server { port, dir, stop, handle: Some(handle) };
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("server did not come up on {port}");
    }

    fn connect(&self) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn cmd(args: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn send(s: &mut TcpStream, args: &[&[u8]]) -> String {
    s.write_all(&cmd(args)).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let n = s.read(&mut buf).unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Poll until both text indexes finish their backfill.
fn wait_ready(c: &mut TcpStream) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let a = send(c, &[b"IDX.QUERY", b"ev.note", b"MATCH", b"warm", b"LIMIT", b"1"]);
        let b = send(c, &[b"IDX.QUERY", b"ctl.note", b"MATCH", b"warm", b"LIMIT", b"1"]);
        if !a.contains("INDEXBUILDING") && !b.contains("INDEXBUILDING") {
            return;
        }
        assert!(Instant::now() < deadline, "indexes never became ready");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn cold_text_answers_byte_identically_to_the_control() {
    let srv = Server::start();
    let mut c = srv.connect();

    // Two tables over ONE prefix: `ev` windowed, `ctl` not. Two text
    // indexes over the same field — the dotted names bind them to
    // their tables, so only ev.note gets a cold directory.
    let declare_ev: &[&[u8]] = &[
        b"TABLE.DECLARE", b"ev", b"PREFIX", b"ev:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64",
        b"INDEX", b"at", b"range",
        b"WINDOW", b"at", b"SPAN", b"100", b"BUCKET", b"10",
    ];
    assert!(send(&mut c, declare_ev).starts_with("+OK"), "declare ev");
    let declare_ctl: &[&[u8]] = &[
        b"TABLE.DECLARE", b"ctl", b"PREFIX", b"ev:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64",
        b"INDEX", b"at", b"range",
    ];
    assert!(send(&mut c, declare_ctl).starts_with("+OK"), "declare ctl");
    for name in [b"ev.note".as_slice(), b"ctl.note"] {
        let r = send(&mut c, &[b"IDX.CREATE", name, b"ON", b"PREFIX", b"ev:",
            b"FIELD", b"note", b"TYPE", b"str", b"KIND", b"text"]);
        assert!(r.starts_with("+OK"), "IDX.CREATE {}: {r}", String::from_utf8_lossy(name));
    }

    // 30 rows, at = i*10; notes cycle a small vocabulary so terms span
    // hot and cold. W = 190 → rows below at=190 freeze.
    let vocab = ["rust engine warm", "storage engine warm", "python glue warm",
                 "rust storage cold path", "engine of record warm"];
    for i in 0..30i64 {
        let key = format!("ev:{i}");
        let at = (i * 10).to_string();
        let note = vocab[(i % 5) as usize];
        let r = send(&mut c, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(),
            b"at", at.as_bytes(), b"note", note.as_bytes()]);
        assert!(r.starts_with(":"), "HSET {key}: {r}");
    }
    wait_ready(&mut c);

    // Wait for the freeze: a txt segment appears.
    let segs = srv.dir.join("segs-0");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frozen = segs.exists()
            && std::fs::read_dir(&segs).is_ok_and(|r| {
                r.filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().starts_with("txt-"))
            });
        if frozen {
            break;
        }
        assert!(Instant::now() < deadline, "text never froze");
        std::thread::sleep(Duration::from_millis(50));
    }

    let compare = |c: &mut TcpStream, tag: &str| {
        for query in [b"rust".as_slice(), b"engine", b"rust storage", b"warm engine", b"absent"] {
            let ev = send(c, &[b"IDX.QUERY", b"ev.note", b"MATCH", query, b"LIMIT", b"30"]);
            let ctl = send(c, &[b"IDX.QUERY", b"ctl.note", b"MATCH", query, b"LIMIT", b"30"]);
            assert_eq!(ev, ctl, "{tag}: MATCH {:?}", String::from_utf8_lossy(query));
        }
    };
    compare(&mut c, "after freeze");

    // Churn: rewrite a cold row's note (revives hot + tombstones the
    // frozen entries), delete another cold row.
    assert!(send(&mut c, &[b"HSET", b"ev:3", b"id", b"ev:3", b"at", b"30",
        b"note", b"rust replaced text"]).starts_with(":"));
    assert_eq!(send(&mut c, &[b"DEL", b"ev:7"]), ":1\r\n");
    std::thread::sleep(Duration::from_millis(200));
    compare(&mut c, "after churn");

    // Clauses refuse on the cold index, serve on the control.
    let f_ev = send(&mut c, &[b"IDX.QUERY", b"ev.note", b"MATCH", b"\"rust engine\"", b"LIMIT", b"5"]);
    assert!(f_ev.contains("not built yet"), "phrase must refuse on cold: {f_ev}");
    let f_ctl = send(&mut c, &[b"IDX.QUERY", b"ctl.note", b"MATCH", b"\"rust engine\"", b"LIMIT", b"5"]);
    assert!(f_ctl.starts_with("*"), "control phrase must serve: {f_ctl}");
    let h_ev = send(&mut c, &[b"IDX.QUERY", b"ev.note", b"MATCH", b"rust", b"LIMIT", b"5", b"HIGHLIGHT"]);
    assert!(h_ev.contains("not built yet"), "highlight must refuse on cold: {h_ev}");
}
