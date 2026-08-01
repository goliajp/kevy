//! The sliding window's semantic-equivalence gate: two tables over the
//! SAME key prefix — one windowed, one not — must answer every scalar
//! range/COUNT byte-identically after the windowed one has evicted its
//! out-of-window prefix into cold segments, including after rewrites,
//! deletes and revivals of cold rows. Clauses on the cold index refuse
//! by name; the control index still serves them.

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
        let dir = std::env::temp_dir().join(format!("kevy-winscalar-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = kevy_config::Config::default();
        cfg.server.port = port;
        cfg.server.threads = 1;
        let state =
            Arc::new(kevy::RuntimeState::new(Arc::new(cfg), dir.clone(), 1).unwrap());
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

#[test]
fn windowed_index_answers_byte_identically_to_the_control() {
    let srv = Server::start();
    let mut c = srv.connect();

    // Two tables over ONE key prefix: `ev` windowed (span 100,
    // bucket 10), `ctl` not. Same rows feed both compiled indexes.
    let declare_ev: &[&[u8]] = &[
        b"TABLE.DECLARE", b"ev", b"PREFIX", b"ev:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64",
        b"INDEX", b"at", b"range", b"VALUES", b"at",
        b"WINDOW", b"at", b"SPAN", b"100", b"BUCKET", b"10",
    ];
    assert!(send(&mut c, declare_ev).starts_with("+OK"), "declare ev");
    let declare_ctl: &[&[u8]] = &[
        b"TABLE.DECLARE", b"ctl", b"PREFIX", b"ev:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64",
        b"INDEX", b"at", b"range", b"VALUES", b"at",
    ];
    assert!(send(&mut c, declare_ctl).starts_with("+OK"), "declare ctl");

    // Rows at=0,10,…,290. max=290 → W = floor((290-100)/10)*10 = 190:
    // rows below 190 (19 of them) leave the hot tree.
    for i in 0..30i64 {
        let key = format!("ev:{i}");
        let at = (i * 10).to_string();
        let r = send(&mut c, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(), b"at", at.as_bytes()]);
        assert!(r.starts_with(":"), "HSET {key}: {r}");
    }

    // Wait for the slide: a derived segment file appears in segs-0/.
    let segs = srv.dir.join("segs-0");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let slid = segs.exists()
            && std::fs::read_dir(&segs)
                .map(|d| d.filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().ends_with(".seg")))
                .unwrap_or(false);
        if slid {
            break;
        }
        assert!(Instant::now() < deadline, "window never slid");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Every shape answers byte-identically: whole domain, cold-only,
    // straddling, hot-only, empty, and single-value EQ on a cold row.
    let ranges: &[(&[u8], &[u8])] = &[
        (b"-1000", b"1000"),
        (b"0", b"100"),
        (b"150", b"250"),
        (b"200", b"300"),
        (b"400", b"500"),
        (b"50", b"50"),
    ];
    let compare = |c: &mut TcpStream, tag: &str| {
        for (lo, hi) in ranges {
            let count_ev = send(c, &[b"IDX.COUNT", b"ev.at", b"RANGE", lo, hi]);
            let count_ctl = send(c, &[b"IDX.COUNT", b"ctl.at", b"RANGE", lo, hi]);
            assert_eq!(count_ev, count_ctl, "{tag}: COUNT {:?}..{:?}", lo, hi);
            let q_ev = send(c, &[b"IDX.QUERY", b"ev.at", b"RANGE", lo, hi, b"LIMIT", b"100"]);
            let q_ctl = send(c, &[b"IDX.QUERY", b"ctl.at", b"RANGE", lo, hi, b"LIMIT", b"100"]);
            assert_eq!(q_ev, q_ctl, "{tag}: QUERY {:?}..{:?}", lo, hi);
        }
    };
    compare(&mut c, "after slide");
    // Ground truth, not just agreement: the whole domain counts 30.
    assert_eq!(send(&mut c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"-1000", b"1000"]), ":30\r\n");

    // Rewrite a cold row (same value), delete another cold row, and
    // revive a third with a new in-window value. The tombstone + bloom
    // machinery must keep the two faces in lockstep.
    assert!(send(&mut c, &[b"HSET", b"ev:5", b"id", b"ev:5", b"at", b"50"]).starts_with(":"));
    assert_eq!(send(&mut c, &[b"DEL", b"ev:7"]), ":1\r\n");
    assert!(send(&mut c, &[b"HSET", b"ev:3", b"id", b"ev:3", b"at", b"260"]).starts_with(":"));
    compare(&mut c, "after cold-row churn");
    assert_eq!(send(&mut c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"-1000", b"1000"]), ":29\r\n");

    // Clauses: the cold index refuses by name; the control serves.
    let f_ev = send(&mut c, &[b"IDX.QUERY", b"ev.at", b"RANGE", b"0", b"300",
        b"FILTER", b"at", b"RANGE", b"100", b"300"]);
    assert!(f_ev.contains("not built yet"), "cold clause must refuse by name: {f_ev}");
    let f_ctl = send(&mut c, &[b"IDX.QUERY", b"ctl.at", b"RANGE", b"0", b"300",
        b"FILTER", b"at", b"RANGE", b"100", b"300"]);
    assert!(f_ctl.starts_with("*"), "control clause must serve: {f_ctl}");
}
