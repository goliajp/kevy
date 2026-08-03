//! The `@` time-expression gate (R4a date arithmetic): every query
//! bound on a declared-i64 column speaks `@now[±dur]` and calendar
//! literals, answering byte-identically to the hand-computed epoch
//! bound — RANGE/EQ, WHERE components, FILTER values. A str column
//! matching a literal "@…" value stays untouched, and a malformed
//! expression refuses rather than matching nothing.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
        let dir = std::env::temp_dir().join(format!("kevy-timebound-{port}"));
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
fn at_expressions_answer_like_their_hand_computed_bounds() {
    let srv = Server::start();
    let mut c = srv.connect();

    // A table whose `at` is epoch SECONDS around the real now — the
    // grammar resolves against the server clock, so the rows must
    // live where @now can see them.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let declare: &[&[u8]] = &[
        b"TABLE.DECLARE", b"ev", b"PREFIX", b"ev:", b"PK", b"id",
        b"COLUMN", b"id", b"str", b"COLUMN", b"at", b"i64", b"COLUMN", b"tag", b"str",
        b"INDEX", b"at", b"range", b"VALUES", b"at", b"tag",
        b"ORDERPATH", b"recent", b"ON", b"at",
    ];
    assert!(send(&mut c, declare).starts_with("+OK"), "declare");
    // 20 rows, one per hour into the past: at = now - i*3600.
    for i in 0..20i64 {
        let key = format!("ev:{i}");
        let at = (now - i * 3600).to_string();
        let tag = if i % 2 == 0 { &b"@now"[..] } else { b"plain" };
        let r = send(&mut c, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(),
            b"at", at.as_bytes(), b"tag", tag]);
        assert!(r.starts_with(":"), "HSET {key}: {r}");
    }

    // Wait for both compiled indexes' backfill.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let a = send(&mut c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"0", b"1"]);
        let b = send(&mut c, &[b"IDX.COUNT", b"ev.recent", b"WHERE", b"RANGE", b"at", b"0", b"1"]);
        if !a.contains("INDEXBUILDING") && !b.contains("INDEXBUILDING") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "indexes never became ready");
        std::thread::sleep(Duration::from_millis(25));
    }

    // Every @ shape against its hand-computed i64 twin. The clock can
    // tick between the two sends, so the bounds are chosen a half-hour
    // off any row's timestamp — a ±few-seconds drift in @now cannot
    // change which rows fall inside.
    let lo6h = (now - 6 * 3600 - 1800).to_string();
    let hi = (now + 1800).to_string();
    type Shape<'a> = &'a [&'a [u8]];
    let pairs: &[(Shape, Shape)] = &[
        (
            &[b"IDX.QUERY", b"ev.at", b"RANGE", b"@now-23400s", b"@now+1800s", b"LIMIT", b"50"],
            &[b"IDX.QUERY", b"ev.at", b"RANGE", lo6h.as_bytes(), hi.as_bytes(), b"LIMIT", b"50"],
        ),
        (
            &[b"IDX.COUNT", b"ev.at", b"RANGE", b"@now-23400s", b"@now+1800s"],
            &[b"IDX.COUNT", b"ev.at", b"RANGE", lo6h.as_bytes(), hi.as_bytes()],
        ),
        (
            &[b"IDX.QUERY", b"ev.recent", b"WHERE", b"RANGE", b"at", b"@now-23400s", b"@now+1800s", b"LIMIT", b"50"],
            &[b"IDX.QUERY", b"ev.recent", b"WHERE", b"RANGE", b"at", lo6h.as_bytes(), hi.as_bytes(), b"LIMIT", b"50"],
        ),
        (
            &[b"IDX.QUERY", b"ev.at", b"RANGE", b"-9000000000000000000", b"9000000000000000000",
              b"LIMIT", b"50", b"FILTER", b"at", b"RANGE", b"@now-23400s", b"@now+1800s"],
            &[b"IDX.QUERY", b"ev.at", b"RANGE", b"-9000000000000000000", b"9000000000000000000",
              b"LIMIT", b"50", b"FILTER", b"at", b"RANGE", lo6h.as_bytes(), hi.as_bytes()],
        ),
    ];
    for (at_form, hand_form) in pairs {
        let a = send(&mut c, at_form);
        let b = send(&mut c, hand_form);
        assert_eq!(a, b, "@-form diverged for {:?}", String::from_utf8_lossy(at_form[2]));
        assert!(a.starts_with("*") || a.starts_with(":"), "shape errored: {a}");
    }
    // The 6.5h window holds rows 0..=6 (7 rows).
    assert_eq!(
        send(&mut c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"@now-23400s", b"@now"]),
        ":7\r\n"
    );
    // Calendar literals parse (bounds far in the past/future).
    let r = send(&mut c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"@2020-01-01", b"@2100-01-01T00:00:00"]);
    assert_eq!(r, ":20\r\n", "literal bounds: {r}");
    // Month arithmetic reaches back too.
    assert_eq!(
        send(&mut c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"@now-1mo", b"@now"]),
        ":20\r\n"
    );

    // A str field holding a literal "@now" is DATA: the EQ value
    // passes through untouched and matches the ten rows carrying it.
    let r = send(&mut c, &[b"IDX.COUNT", b"ev.at", b"RANGE", b"-9000000000000000000",
        b"9000000000000000000", b"FILTER", b"tag", b"EQ", b"@now"]);
    assert_eq!(r, ":10\r\n", "str @ value must stay data: {r}");

    // Malformed expressions refuse by name, never match-nothing.
    for bad in [&b"@now-7q"[..], b"@later", b"@2026-02-30"] {
        let r = send(&mut c, &[b"IDX.QUERY", b"ev.at", b"RANGE", bad, b"@now", b"LIMIT", b"5"]);
        assert!(r.starts_with("-ERR"), "{:?} must refuse: {r}", String::from_utf8_lossy(bad));
    }
}
