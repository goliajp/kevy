//! v2.5 IDX.* — end-to-end against a real 8-shard reactor: rows land
//! on different shards, so IDX.QUERY exercises the extension fan-out
//! merge, the global (value,key) cursor, and the tick-driven backfill.

use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

fn req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

fn cmd(s: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    s.write_all(&req(parts)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let mut buf = [0u8; 65536];
    let n = s.read(&mut buf).unwrap();
    buf[..n].to_vec()
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-idx-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, 8, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..400 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        s
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Poll IDX.QUERY until backfill finishes (ticks run at ~10 Hz).
fn query_ready(c: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    for _ in 0..100 {
        let r = cmd(c, parts);
        if !r.starts_with(b"-INDEXBUILDING") {
            return r;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("index never became ready");
}

#[test]
fn create_backfill_query_cursor_count_verify_drop() {
    let srv = Server::start();
    let mut c = srv.connect();

    // Pre-existing rows across shards (to be backfilled).
    for i in 0..40 {
        cmd(
            &mut c,
            &[b"HSET", format!("user:{i}").as_bytes(), b"age", format!("{}", 20 + i).as_bytes()],
        );
    }
    cmd(&mut c, &[b"HSET", b"user:bad", b"age", b"not-a-number"]);
    cmd(&mut c, &[b"SET", b"other:1", b"x"]); // outside the domain

    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"age_idx", b"ON", b"PREFIX", b"user:", b"FIELD", b"age", b"TYPE", b"i64", b"KIND", b"range"],
    );
    assert_eq!(r, b"+OK\r\n");

    // Live write during build (double-write path).
    cmd(&mut c, &[b"HSET", b"user:live", b"age", b"18"]);

    // Query: ages [18, 30] → live(18) + rows 20..=30 = 12 hits.
    let r = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"age_idx", b"RANGE", b"18", b"30", b"LIMIT", b"100"],
    );
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("user:live"), "live-written row present: {s}");
    assert!(s.contains("user:0"), "backfilled row present: {s}");
    assert!(!s.contains("user:bad"), "coerce-failed row excluded: {s}");
    let hits = s.matches("user:").count();
    assert_eq!(hits, 12, "{s}");

    // Cursor pagination: LIMIT 5 then resume; pages don't overlap.
    let r1 = cmd(&mut c, &[b"IDX.QUERY", b"age_idx", b"RANGE", b"18", b"30", b"LIMIT", b"5"]);
    let s1 = String::from_utf8_lossy(&r1);
    let cursor = s1.lines().nth(2).unwrap().to_string(); // first bulk = cursor
    assert_ne!(cursor, "0", "more pages: {s1}");
    let r2 = cmd(
        &mut c,
        &[b"IDX.QUERY", b"age_idx", b"RANGE", b"18", b"30", b"LIMIT", b"100", b"CURSOR", cursor.as_bytes()],
    );
    let s2 = String::from_utf8_lossy(&r2);
    assert_eq!(s1.matches("user:").count(), 5);
    assert_eq!(s2.matches("user:").count(), 7, "5 + 7 = 12: {s2}");
    for key in s1.lines().filter(|l| l.starts_with("user:")) {
        assert!(!s2.contains(&format!("{key}\r")), "no overlap on {key}");
    }

    // COUNT + EQ + VERIFY + LIST.
    let r = cmd(&mut c, &[b"IDX.COUNT", b"age_idx", b"RANGE", b"18", b"30"]);
    assert_eq!(r, b":12\r\n");
    let r = cmd(&mut c, &[b"IDX.QUERY", b"age_idx", b"EQ", b"25", b"LIMIT", b"10"]);
    assert!(String::from_utf8_lossy(&r).contains("user:5"), "20+5=25");
    let r = cmd(&mut c, &[b"IDX.VERIFY", b"age_idx"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("entries\r\n$2\r\n41"), "40 + live: {s}");
    assert!(s.contains("coerce_failures\r\n$1\r\n1"), "{s}");
    let r = cmd(&mut c, &[b"IDX.LIST"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("age_idx") && s.contains("ready"), "{s}");

    // Update moves a row out of range; delete removes.
    cmd(&mut c, &[b"HSET", b"user:live", b"age", b"99"]);
    cmd(&mut c, &[b"DEL", b"user:0"]);
    let r = cmd(&mut c, &[b"IDX.COUNT", b"age_idx", b"RANGE", b"18", b"30"]);
    assert_eq!(r, b":10\r\n", "live moved out, user:0 gone");

    // Errors + DROP.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"nope", b"EQ", b"1"]);
    assert!(r.starts_with(b"-ERR no such index"));
    assert_eq!(cmd(&mut c, &[b"IDX.DROP", b"age_idx"]), b":1\r\n");
    assert_eq!(cmd(&mut c, &[b"IDX.DROP", b"age_idx"]), b":0\r\n");
    let r = cmd(&mut c, &[b"IDX.QUERY", b"age_idx", b"EQ", b"25"]);
    assert!(r.starts_with(b"-ERR no such index"));
}
