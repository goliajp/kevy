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

use kevy_testnet::free_port;

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
        b"COLUMN", b"prio", b"i64", b"COLUMN", b"tag", b"str",
        b"INDEX", b"at", b"range", b"VALUES", b"at", b"prio", b"tag",
        b"WINDOW", b"at", b"SPAN", b"100", b"BUCKET", b"10",
    ];
    assert!(send(&mut c, declare_ev).starts_with("+OK"), "declare ev");
    let declare_ctl: &[&[u8]] = &[
        b"TABLE.DECLARE", b"ctl", b"PREFIX", b"ev:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64",
        b"COLUMN", b"prio", b"i64", b"COLUMN", b"tag", b"str",
        b"INDEX", b"at", b"range", b"VALUES", b"at", b"prio", b"tag",
    ];
    assert!(send(&mut c, declare_ctl).starts_with("+OK"), "declare ctl");

    // Rows at=0,10,…,290. max=290 → W = floor((290-100)/10)*10 = 190:
    // rows below 190 (19 of them) leave the hot tree. prio cycles for
    // SORT/FILTER ties, tag for DISTINCT/FACET groups — and every 7th
    // row has NO tag, so the absent-value rules (never filters, sorts
    // last, own group) run on both faces.
    let tags = ["alpha", "beta", "gamma"];
    for i in 0..30i64 {
        let key = format!("ev:{i}");
        let at = (i * 10).to_string();
        let prio = ((i % 4) * 10).to_string();
        let mut argv: Vec<&[u8]> = vec![b"HSET", key.as_bytes(), b"id", key.as_bytes(),
            b"at", at.as_bytes(), b"prio", prio.as_bytes()];
        let tag = tags[(i % 3) as usize];
        if i % 7 != 0 {
            argv.extend_from_slice(&[b"tag", tag.as_bytes()]);
        }
        let r = send(&mut c, &argv);
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
    // …and every clause with a cold path, over the straddling range:
    // FILTER (numeric + string EQ), SORT (both directions), DISTINCT,
    // FACET, OFFSET, their combinations, and the claused COUNT.
    let clause_shapes: &[&[&[u8]]] = &[
        &[b"FILTER", b"prio", b"RANGE", b"10", b"20"],
        &[b"FILTER", b"tag", b"EQ", b"beta"],
        &[b"FILTER", b"at", b"RANGE", b"100", b"250"],
        &[b"SORT", b"prio", b"ASC"],
        &[b"SORT", b"prio", b"DESC"],
        &[b"SORT", b"tag", b"ASC"],
        &[b"DISTINCT", b"tag"],
        &[b"DISTINCT", b"prio", b"SORT", b"prio", b"DESC"],
        &[b"FACET", b"tag"],
        &[b"FACET", b"prio", b"FILTER", b"tag", b"EQ", b"alpha"],
        &[b"OFFSET", b"3"],
        &[b"FILTER", b"prio", b"RANGE", b"0", b"20", b"SORT", b"prio", b"ASC", b"OFFSET", b"2"],
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
        for shape in clause_shapes {
            let mut ev: Vec<&[u8]> =
                vec![b"IDX.QUERY", b"ev.at", b"RANGE", b"0", b"280", b"LIMIT", b"100"];
            ev.extend_from_slice(shape);
            let mut ctl: Vec<&[u8]> =
                vec![b"IDX.QUERY", b"ctl.at", b"RANGE", b"0", b"280", b"LIMIT", b"100"];
            ctl.extend_from_slice(shape);
            let ev = send(c, &ev);
            let ctl = send(c, &ctl);
            assert_eq!(ev, ctl, "{tag}: clauses {:?}", String::from_utf8_lossy(shape[0]));
            assert!(ctl.starts_with("*"), "{tag}: control refused: {ctl}");
        }
        let cc_ev = send(c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"0", b"280",
            b"FILTER", b"prio", b"RANGE", b"10", b"20"]);
        let cc_ctl = send(c, &[b"IDX.COUNT", b"ctl.at", b"RANGE", b"0", b"280",
            b"FILTER", b"prio", b"RANGE", b"10", b"20"]);
        assert_eq!(cc_ev, cc_ctl, "{tag}: claused COUNT");
        assert!(cc_ctl.starts_with(":"), "{tag}: claused COUNT refused: {cc_ctl}");
        // FILTER + CURSOR pages the merged cold+hot stream: walk both
        // faces page by page — every page (cursor included) byte-equal,
        // and the walk terminates.
        let mut cursor = String::new();
        for page in 0..20 {
            let mut ev: Vec<&[u8]> = vec![b"IDX.QUERY", b"ev.at", b"RANGE", b"0", b"280",
                b"LIMIT", b"4", b"FILTER", b"prio", b"RANGE", b"0", b"20"];
            let mut ctl: Vec<&[u8]> = vec![b"IDX.QUERY", b"ctl.at", b"RANGE", b"0", b"280",
                b"LIMIT", b"4", b"FILTER", b"prio", b"RANGE", b"0", b"20"];
            if !cursor.is_empty() {
                ev.extend_from_slice(&[b"CURSOR", cursor.as_bytes()]);
                ctl.extend_from_slice(&[b"CURSOR", cursor.as_bytes()]);
            }
            let ev = send(c, &ev);
            let ctl = send(c, &ctl);
            assert_eq!(ev, ctl, "{tag}: paged FILTER page {page}");
            cursor = ev.lines().nth(2).unwrap_or("0").to_string();
            if cursor == "0" {
                break;
            }
            assert!(page < 19, "{tag}: paged FILTER never terminated");
        }
        assert_eq!(cursor, "0", "{tag}: paged FILTER ended cleanly");
        // …and the PLAIN range pages the same way (the cursor goes
        // into the cold walk — a post-filter starves the cold side on
        // page 2+, the orderpath e2e's catch).
        let mut cursor = String::new();
        for page in 0..20 {
            let mut ev: Vec<&[u8]> = vec![b"IDX.QUERY", b"ev.at",
                b"RANGE", b"0", b"280", b"LIMIT", b"4"];
            let mut ctl: Vec<&[u8]> = vec![b"IDX.QUERY", b"ctl.at",
                b"RANGE", b"0", b"280", b"LIMIT", b"4"];
            if !cursor.is_empty() {
                ev.extend_from_slice(&[b"CURSOR", cursor.as_bytes()]);
                ctl.extend_from_slice(&[b"CURSOR", cursor.as_bytes()]);
            }
            let ev = send(c, &ev);
            let ctl = send(c, &ctl);
            assert_eq!(ev, ctl, "{tag}: plain page {page}");
            cursor = ev.lines().nth(2).unwrap_or("0").to_string();
            if cursor == "0" {
                break;
            }
            assert!(page < 19, "{tag}: plain paging never terminated");
        }
        assert_eq!(cursor, "0", "{tag}: plain paging ended cleanly");
    };
    compare(&mut c, "after slide");
    // Ground truth, not just agreement: the whole domain counts 30.
    assert_eq!(send(&mut c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"-1000", b"1000"]), ":30\r\n");

    // Rewrite a cold row (same driving value, NEW stored values — the
    // frozen payload for it must stop serving), delete another cold
    // row, and revive a third with a new in-window value. The
    // tombstone + bloom machinery must keep the two faces in lockstep.
    assert!(send(&mut c, &[b"HSET", b"ev:5", b"id", b"ev:5", b"at", b"50",
        b"prio", b"99", b"tag", b"delta"]).starts_with(":"));
    assert_eq!(send(&mut c, &[b"DEL", b"ev:7"]), ":1\r\n");
    assert!(send(&mut c, &[b"HSET", b"ev:3", b"id", b"ev:3", b"at", b"260",
        b"prio", b"30", b"tag", b"beta"]).starts_with(":"));
    compare(&mut c, "after cold-row churn");
    assert_eq!(send(&mut c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"-1000", b"1000"]), ":29\r\n");
}
