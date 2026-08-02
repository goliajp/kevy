//! The text window's semantic-equivalence gate: a windowed table's
//! TEXT index freezes its out-of-window documents into cold bucket
//! segments, and MATCH answers — scores, sort keys, facet counts and
//! highlight spans included — must be byte-identical to a
//! never-windowed control over the same rows, for every clause with a
//! cold path: terms, phrases, FILTER, SORT, DISTINCT, FACET and
//! HIGHLIGHT. The dictionary-shaped clauses (prefix, TYPO, IN) refuse
//! by name while cold buckets exist.

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
            b"FIELD", b"note", b"TYPE", b"str", b"KIND", b"text",
            b"WITH", b"POSITIONS",
            b"VALUES", b"prio", b"tag", b"TYPES", b"i64", b"str"]);
        assert!(r.starts_with("+OK"), "IDX.CREATE {}: {r}", String::from_utf8_lossy(name));
    }

    // 30 rows, at = i*10; notes cycle a small vocabulary so terms AND
    // the "rust engine" phrase span hot and cold. W = 190 → rows below
    // at=190 freeze. prio cycles for SORT/FILTER ties, tag for
    // DISTINCT/FACET groups — and every 7th row has NO tag, so the
    // absent-value rules (never filters, sorts last, own group) are
    // exercised on both faces.
    let vocab = ["rust engine warm", "storage engine warm", "python glue warm",
                 "rust storage cold path", "engine of record warm"];
    let tags = ["alpha", "beta", "gamma"];
    for i in 0..30i64 {
        let key = format!("ev:{i}");
        let at = (i * 10).to_string();
        let note = vocab[(i % 5) as usize];
        let prio = ((i % 4) * 10).to_string();
        let mut argv: Vec<&[u8]> = vec![b"HSET", key.as_bytes(), b"id", key.as_bytes(),
            b"at", at.as_bytes(), b"note", note.as_bytes(), b"prio", prio.as_bytes()];
        let tag = tags[(i % 3) as usize];
        if i % 7 != 0 {
            argv.extend_from_slice(&[b"tag", tag.as_bytes()]);
        }
        let r = send(&mut c, &argv);
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

    // Every clause with a cold path, byte-for-byte against the
    // control: terms, phrases (adjacency verified through the frozen
    // positions blobs), FILTER, SORT (both directions), DISTINCT,
    // FACET, HIGHLIGHT, and their combinations.
    let shapes: &[&[&[u8]]] = &[
        &[b"MATCH", b"rust", b"LIMIT", b"30"],
        &[b"MATCH", b"engine", b"LIMIT", b"30"],
        &[b"MATCH", b"rust storage", b"LIMIT", b"30"],
        &[b"MATCH", b"warm engine", b"LIMIT", b"30"],
        &[b"MATCH", b"absent", b"LIMIT", b"30"],
        &[b"MATCH", b"\"rust engine\"", b"LIMIT", b"30"],
        &[b"MATCH", b"\"storage engine\"", b"LIMIT", b"30"],
        &[b"MATCH", b"warm \"rust storage\"", b"LIMIT", b"30"],
        &[b"MATCH", b"rust", b"LIMIT", b"30", b"FILTER", b"prio", b"RANGE", b"10", b"20"],
        &[b"MATCH", b"engine", b"LIMIT", b"30", b"FILTER", b"tag", b"EQ", b"alpha"],
        &[b"MATCH", b"rust", b"LIMIT", b"30", b"SORT", b"prio", b"ASC"],
        &[b"MATCH", b"rust", b"LIMIT", b"30", b"SORT", b"prio", b"DESC"],
        &[b"MATCH", b"engine", b"LIMIT", b"30", b"SORT", b"tag", b"ASC"],
        &[b"MATCH", b"engine", b"LIMIT", b"30", b"DISTINCT", b"tag"],
        &[b"MATCH", b"rust", b"LIMIT", b"30", b"DISTINCT", b"prio", b"SORT", b"prio", b"DESC"],
        &[b"MATCH", b"rust", b"LIMIT", b"1", b"FACET", b"tag"],
        &[b"MATCH", b"engine", b"LIMIT", b"1", b"FACET", b"prio",
          b"FILTER", b"tag", b"EQ", b"beta"],
        &[b"MATCH", b"rust", b"LIMIT", b"30", b"HIGHLIGHT"],
        &[b"MATCH", b"\"rust engine\"", b"LIMIT", b"30", b"HIGHLIGHT"],
        &[b"MATCH", b"rust", b"LIMIT", b"30", b"FILTER", b"prio", b"RANGE", b"0", b"20",
          b"SORT", b"prio", b"ASC", b"HIGHLIGHT"],
    ];
    let compare = |c: &mut TcpStream, tag: &str| {
        for shape in shapes {
            let mut ev: Vec<&[u8]> = vec![b"IDX.QUERY", b"ev.note"];
            ev.extend_from_slice(shape);
            let mut ctl: Vec<&[u8]> = vec![b"IDX.QUERY", b"ctl.note"];
            ctl.extend_from_slice(shape);
            let ev = send(c, &ev);
            let ctl = send(c, &ctl);
            assert_eq!(ev, ctl, "{tag}: {:?}", String::from_utf8_lossy(shape[1]));
        }
    };
    compare(&mut c, "after freeze");

    // Churn: rewrite a cold row's note AND values (revives hot +
    // tombstones the frozen entries, statistics withdrawn exactly),
    // delete another cold row.
    assert!(send(&mut c, &[b"HSET", b"ev:3", b"id", b"ev:3", b"at", b"30",
        b"note", b"rust replaced text", b"prio", b"5", b"tag", b"beta"]).starts_with(":"));
    assert_eq!(send(&mut c, &[b"DEL", b"ev:7"]), ":1\r\n");
    std::thread::sleep(Duration::from_millis(200));
    compare(&mut c, "after churn");

    // The dictionary-shaped clauses refuse on the cold index by name,
    // and serve on the control.
    let refusals: &[&[&[u8]]] = &[
        &[b"MATCH", b"rus*", b"LIMIT", b"5"],
        &[b"MATCH", b"rusk", b"LIMIT", b"5", b"TYPO", b"1"],
        &[b"MATCH", b"rust", b"LIMIT", b"5", b"IN", b"note"],
    ];
    for shape in refusals {
        let mut ev: Vec<&[u8]> = vec![b"IDX.QUERY", b"ev.note"];
        ev.extend_from_slice(shape);
        let mut ctl: Vec<&[u8]> = vec![b"IDX.QUERY", b"ctl.note"];
        ctl.extend_from_slice(shape);
        let ev = send(&mut c, &ev);
        assert!(ev.contains("not built yet"),
            "must refuse on cold: {:?} -> {ev}", String::from_utf8_lossy(shape[1]));
        let ctl = send(&mut c, &ctl);
        assert!(ctl.starts_with("*"),
            "control must serve: {:?} -> {ctl}", String::from_utf8_lossy(shape[1]));
    }
}
