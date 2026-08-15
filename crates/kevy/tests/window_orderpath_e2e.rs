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
fn windowed_orderpath_answers_byte_identically_to_the_control() {
    let srv = Server::start();
    let mut c = srv.connect();

    // Two tables over ONE key prefix, each with the window column's
    // INDEX (the row-eviction driver) AND an ORDERPATH it leads
    // ascending: `ev` windowed, `ctl` not. The orderpath's composite
    // tree must slide its own prefix and keep answering WHERE queries
    // byte-identically through the cold segments.
    let declare_ev: &[&[u8]] = &[
        b"TABLE.DECLARE", b"ev", b"PREFIX", b"ev:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64", b"COLUMN", b"prio", b"i64",
        b"INDEX", b"at", b"range",
        b"ORDERPATH", b"recent", b"ON", b"at", b"THEN", b"prio", b"DESC",
        b"WINDOW", b"at", b"SPAN", b"100", b"BUCKET", b"10",
    ];
    assert!(send(&mut c, declare_ev).starts_with("+OK"), "declare ev");
    let declare_ctl: &[&[u8]] = &[
        b"TABLE.DECLARE", b"ctl", b"PREFIX", b"ev:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64", b"COLUMN", b"prio", b"i64",
        b"INDEX", b"at", b"range",
        b"ORDERPATH", b"recent", b"ON", b"at", b"THEN", b"prio", b"DESC",
    ];
    assert!(send(&mut c, declare_ctl).starts_with("+OK"), "declare ctl");

    // Rows at=0,10,…,290 (max=290 → W=190: 19 rows go cold); prio
    // cycles so the DESC second component reorders within an at-tie
    // (two rows per at value, i and i+30 sharing at).
    for i in 0..30i64 {
        for (suffix, base) in [("a", 0i64), ("b", 1)] {
            let key = format!("ev:{i}{suffix}");
            let at = (i * 10).to_string();
            let prio = ((i + base) % 5).to_string();
            let r = send(&mut c, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(),
                b"at", at.as_bytes(), b"prio", prio.as_bytes()]);
            assert!(r.starts_with(":"), "HSET {key}: {r}");
        }
    }

    // Wait for the slide: TWO derived segment families appear (the
    // window-column index's and the orderpath's).
    let segs = srv.dir.join("segs-0");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let n = std::fs::read_dir(&segs)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().starts_with("idx-"))
                    .count()
            })
            .unwrap_or(0);
        if n >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "orderpath window never slid ({n} families)");
        std::thread::sleep(Duration::from_millis(50));
    }

    // WHERE shapes: whole domain, cold-only, straddling, hot-only,
    // empty, EQ on a cold at-value (its prio-DESC page order must
    // survive the freeze) — plus COUNT and pagination.
    let wheres: &[&[&[u8]]] = &[
        &[b"WHERE", b"RANGE", b"at", b"-1000", b"1000"],
        &[b"WHERE", b"RANGE", b"at", b"0", b"100"],
        &[b"WHERE", b"RANGE", b"at", b"150", b"250"],
        &[b"WHERE", b"RANGE", b"at", b"200", b"300"],
        &[b"WHERE", b"RANGE", b"at", b"400", b"500"],
        &[b"WHERE", b"at", b"EQ", b"50"],
        &[b"WHERE", b"at", b"EQ", b"50", b"RANGE", b"prio", b"1", b"3"],
    ];
    let compare = |c: &mut TcpStream, tag: &str| {
        for shape in wheres {
            let mut ev: Vec<&[u8]> = vec![b"IDX.QUERY", b"ev.recent"];
            ev.extend_from_slice(shape);
            ev.extend_from_slice(&[b"LIMIT", b"100"]);
            let mut ctl: Vec<&[u8]> = vec![b"IDX.QUERY", b"ctl.recent"];
            ctl.extend_from_slice(shape);
            ctl.extend_from_slice(&[b"LIMIT", b"100"]);
            let ev = send(c, &ev);
            let ctl = send(c, &ctl);
            assert_eq!(ev, ctl, "{tag}: QUERY {:?}", String::from_utf8_lossy(shape[1]));
            assert!(ctl.starts_with("*"), "{tag}: control refused: {ctl}");
            let mut cev: Vec<&[u8]> = vec![b"IDX.COUNT", b"ev.recent"];
            cev.extend_from_slice(shape);
            let mut cctl: Vec<&[u8]> = vec![b"IDX.COUNT", b"ctl.recent"];
            cctl.extend_from_slice(shape);
            assert_eq!(send(c, &cev), send(c, &cctl), "{tag}: COUNT");
        }
        // Cross-window pagination: page LIMIT 7 through the straddling
        // range on both faces; every page (cursor included) byte-equal.
        let mut cursor = String::new();
        for page in 0..20 {
            let mut ev: Vec<&[u8]> = vec![b"IDX.QUERY", b"ev.recent",
                b"WHERE", b"RANGE", b"at", b"0", b"280", b"LIMIT", b"7"];
            let mut ctl: Vec<&[u8]> = vec![b"IDX.QUERY", b"ctl.recent",
                b"WHERE", b"RANGE", b"at", b"0", b"280", b"LIMIT", b"7"];
            if !cursor.is_empty() {
                ev.extend_from_slice(&[b"CURSOR", cursor.as_bytes()]);
                ctl.extend_from_slice(&[b"CURSOR", cursor.as_bytes()]);
            }
            let ev = send(c, &ev);
            let ctl = send(c, &ctl);
            assert_eq!(ev, ctl, "{tag}: page {page}");
            cursor = ev.lines().nth(2).unwrap_or("0").to_string();
            if cursor == "0" {
                break;
            }
            assert!(page < 19, "{tag}: pagination never terminated");
        }
        assert_eq!(cursor, "0", "{tag}: pagination ended cleanly");
    };
    compare(&mut c, "after slide");
    assert_eq!(
        send(&mut c, &[b"IDX.COUNT", b"ev.recent", b"WHERE", b"RANGE", b"at", b"-1000", b"1000"]),
        ":60\r\n"
    );

    // Cold-row churn: rewrite (new prio — the frozen composite entry
    // must stop serving), delete, revive in-window.
    assert!(send(&mut c, &[b"HSET", b"ev:5a", b"id", b"ev:5a", b"at", b"50",
        b"prio", b"4"]).starts_with(":"));
    assert_eq!(send(&mut c, &[b"DEL", b"ev:7b"]), ":1\r\n");
    assert!(send(&mut c, &[b"HSET", b"ev:3a", b"id", b"ev:3a", b"at", b"260",
        b"prio", b"0"]).starts_with(":"));
    compare(&mut c, "after cold-row churn");
    assert_eq!(
        send(&mut c, &[b"IDX.COUNT", b"ev.recent", b"WHERE", b"RANGE", b"at", b"-1000", b"1000"]),
        ":59\r\n"
    );
}

#[test]
fn orderpath_only_window_slides_rows_and_serves() {
    let srv = Server::start();
    let mut c = srv.connect();

    // No INDEX on the window column at all: the ASC-led orderpath is
    // the table's only windowed access path, so IT drives row
    // eviction — the declaration validation promised this shape works.
    let declare_ev: &[&[u8]] = &[
        b"TABLE.DECLARE", b"op", b"PREFIX", b"op:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64",
        b"ORDERPATH", b"recent", b"ON", b"at",
        b"WINDOW", b"at", b"SPAN", b"100", b"BUCKET", b"10",
    ];
    assert!(send(&mut c, declare_ev).starts_with("+OK"), "declare op");
    let declare_ctl: &[&[u8]] = &[
        b"TABLE.DECLARE", b"opc", b"PREFIX", b"op:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64",
        b"ORDERPATH", b"recent", b"ON", b"at",
    ];
    assert!(send(&mut c, declare_ctl).starts_with("+OK"), "declare opc");
    for i in 0..30i64 {
        let key = format!("op:{i}");
        let at = (i * 10).to_string();
        let r = send(&mut c, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(),
            b"at", at.as_bytes()]);
        assert!(r.starts_with(":"), "HSET {key}: {r}");
    }

    // Rows really evict (a row-… segment appears) AND the orderpath's
    // own derived segment appears — the driver did both jobs.
    let segs = srv.dir.join("segs-0");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (mut idx, mut row) = (false, false);
        if let Ok(d) = std::fs::read_dir(&segs) {
            for e in d.filter_map(Result::ok) {
                let n = e.file_name().to_string_lossy().into_owned();
                idx |= n.starts_with("idx-");
                row |= n.starts_with("row-");
            }
        }
        if idx && row {
            break;
        }
        assert!(Instant::now() < deadline, "orderpath-only window never slid");
        std::thread::sleep(Duration::from_millis(50));
    }

    // KV transparency across the phase change, and WHERE equivalence.
    let g = send(&mut c, &[b"HGET", b"op:0", b"at"]);
    assert_eq!(g, "$1\r\n0\r\n", "cold row still answers: {g}");
    let shapes: &[&[&[u8]]] = &[
        &[b"WHERE", b"RANGE", b"at", b"-1000", b"1000"],
        &[b"WHERE", b"RANGE", b"at", b"0", b"100"],
        &[b"WHERE", b"RANGE", b"at", b"150", b"250"],
    ];
    for shape in shapes {
        let mut ev: Vec<&[u8]> = vec![b"IDX.QUERY", b"op.recent"];
        ev.extend_from_slice(shape);
        ev.extend_from_slice(&[b"LIMIT", b"100"]);
        let mut ctl: Vec<&[u8]> = vec![b"IDX.QUERY", b"opc.recent"];
        ctl.extend_from_slice(shape);
        ctl.extend_from_slice(&[b"LIMIT", b"100"]);
        assert_eq!(send(&mut c, &ev), send(&mut c, &ctl), "WHERE equivalence");
    }
    assert_eq!(
        send(&mut c, &[b"IDX.COUNT", b"op.recent", b"WHERE", b"RANGE", b"at", b"-1000", b"1000"]),
        ":30\r\n"
    );
}
