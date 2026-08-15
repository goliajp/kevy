//! VIEW.* — end-to-end against a real 8-shard reactor.

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

/// Read exactly ONE complete RESP value (types parsed recursively) —
/// a single timed recv desyncs when a reply lands late.
fn read_value(s: &mut std::net::TcpStream, buf: &mut Vec<u8>, out: &mut Vec<u8>) {
    fn read_line(s: &mut std::net::TcpStream, buf: &mut Vec<u8>, out: &mut Vec<u8>) -> Vec<u8> {
        loop {
            if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                let line: Vec<u8> = buf.drain(..pos + 2).collect();
                out.extend_from_slice(&line);
                return line;
            }
            let mut tmp = [0u8; 65536];
            let n = s.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
        }
    }
    let line = read_line(s, buf, out);
    match line[0] {
        b'+' | b'-' | b':' => {}
        b'$' => {
            let n: i64 = std::str::from_utf8(&line[1..line.len() - 2]).unwrap().parse().unwrap();
            if n >= 0 {
                let want = n as usize + 2;
                while buf.len() < want {
                    let mut tmp = [0u8; 65536];
                    let got = s.read(&mut tmp).unwrap();
                    buf.extend_from_slice(&tmp[..got]);
                }
                let payload: Vec<u8> = buf.drain(..want).collect();
                out.extend_from_slice(&payload);
            }
        }
        b'*' => {
            let n: i64 = std::str::from_utf8(&line[1..line.len() - 2]).unwrap().parse().unwrap();
            for _ in 0..n.max(0) {
                read_value(s, buf, out);
            }
        }
        _ => panic!("unexpected RESP head: {line:?}"),
    }
}

fn cmd(s: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    s.write_all(&req(parts)).unwrap();
    thread_local! {
        static RESIDUE: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    RESIDUE.with(|r| {
        let mut buf = r.borrow_mut();
        let mut out = Vec::new();
        read_value(s, &mut buf, &mut out);
        out
    })
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// NB: the view/index catalogs are process-global statics — tests
    /// in this binary must NOT run concurrently. Each test holds the
    /// START_GATE guard for its whole body via [`Server::start`]'s
    /// returned guard.
    fn start() -> (Self, std::sync::MutexGuard<'static, ()>) {
        let gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-view-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(8)).bind([127, 0, 0, 1], port).shards(8)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
        (Self { port, dir, stop, handle: Some(handle) }, gate)
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

fn ready(c: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    for _ in 0..100 {
        let r = cmd(c, parts);
        if !r.starts_with(b"-INDEXBUILDING") {
            return r;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("still building");
}

fn seed(c: &mut std::net::TcpStream) {
    for i in 0..30 {
        cmd(
            c,
            &[
                b"HSET", format!("job:{i}").as_bytes(),
                b"pri", format!("{i}").as_bytes(),
                b"state", if i % 3 == 0 { b"ready" } else { b"done" },
            ],
        );
    }
    cmd(&mut *c, &[b"IDX.CREATE", b"j_pri", b"ON", b"PREFIX", b"job:", b"FIELD", b"pri", b"TYPE", b"i64", b"KIND", b"range"]);
    cmd(&mut *c, &[b"IDX.CREATE", b"j_state", b"ON", b"PREFIX", b"job:", b"FIELD", b"state", b"TYPE", b"str", b"KIND", b"range"]);
}

#[test]
fn virtual_view_query_order_cursor() {
    let (srv, _gate) = Server::start();
    let mut c = srv.connect();
    seed(&mut c);
    // ready jobs with pri in [0, 20], ordered by pri
    let r = cmd(
        &mut c,
        &[b"VIEW.CREATE", b"v_ready", b"QUERY", b"(", b"AND",
          b"j_pri", b"RANGE", b"0", b"20", b"j_state", b"EQ", b"ready", b")",
          b"ORDER", b"BY", b"j_pri"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));
    // members: i%3==0 && i<=20 → 0,3,6,9,12,15,18 = 7
    let r = ready(&mut c, &[b"VIEW.QUERY", b"v_ready", b"LIMIT", b"100"]);
    let s = String::from_utf8_lossy(&r);
    assert_eq!(s.matches("job:").count(), 7, "{s}");
    assert!(s.find("job:0\r").unwrap() < s.find("job:18\r").unwrap(), "ascending: {s}");
    // cursor pages disjoint
    let r1 = cmd(&mut c, &[b"VIEW.QUERY", b"v_ready", b"LIMIT", b"3"]);
    let s1 = String::from_utf8_lossy(&r1);
    let cur = s1.lines().nth(2).unwrap().to_string();
    let r2 = cmd(&mut c, &[b"VIEW.QUERY", b"v_ready", b"LIMIT", b"100", b"CURSOR", cur.as_bytes()]);
    let s2 = String::from_utf8_lossy(&r2);
    assert_eq!(s1.matches("job:").count() + s2.matches("job:").count(), 7);
    // EXPLAIN reports the tree + leaf counts
    let r = cmd(&mut c, &[b"VIEW.EXPLAIN", b"v_ready"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("(AND j_pri[..] j_state[..])"), "{s}");
    assert!(s.contains("21,10"), "leaf counts 21 and 10: {s}");
    // live updates visible immediately (virtual)
    cmd(&mut c, &[b"HSET", b"job:1", b"state", b"ready"]);
    let r = cmd(&mut c, &[b"VIEW.QUERY", b"v_ready", b"LIMIT", b"100"]);
    assert_eq!(String::from_utf8_lossy(&r).matches("job:").count(), 8);
}

#[test]
fn materialized_topk_desc_maintenance() {
    let (srv, _gate) = Server::start();
    let mut c = srv.connect();
    seed(&mut c);
    // top-5 highest-pri ready jobs (DESC + TOPK)
    let r = cmd(
        &mut c,
        &[b"VIEW.CREATE", b"v_top", b"QUERY", b"(", b"AND",
          b"j_pri", b"RANGE", b"0", b"100", b"j_state", b"EQ", b"ready", b")",
          b"ORDER", b"BY", b"j_pri", b"DESC", b"MODE", b"materialized", b"TOPK", b"5"],
    );
    assert_eq!(r, b"+OK\r\n");
    // ready: 0,3,...,27 → top-5 desc = 27,24,21,18,15
    let r = ready(&mut c, &[b"VIEW.QUERY", b"v_top", b"LIMIT", b"5"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("job:27") && s.contains("job:15"), "{s}");
    assert!(!s.contains("job:12\r"), "{s}");
    assert!(s.find("job:27\r").unwrap() < s.find("job:15\r").unwrap(), "desc: {s}");

    // incremental: a new high-pri ready job enters the top
    cmd(&mut c, &[b"HSET", b"job:99", b"pri", b"99", b"state", b"ready"]);
    let r = cmd(&mut c, &[b"VIEW.QUERY", b"v_top", b"LIMIT", b"3"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("job:99"), "{s}");
    // a member leaving (state flips) drops out
    cmd(&mut c, &[b"HSET", b"job:27", b"state", b"done"]);
    let r = cmd(&mut c, &[b"VIEW.QUERY", b"v_top", b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    assert!(!s.contains("job:27\r"), "{s}");
    // VERIFY reports members; REBUILD keeps answers identical
    let r = cmd(&mut c, &[b"VIEW.VERIFY", b"v_top"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("members"), "{s}");
    let before = cmd(&mut c, &[b"VIEW.QUERY", b"v_top", b"LIMIT", b"10"]);
    assert_eq!(cmd(&mut c, &[b"VIEW.REBUILD", b"v_top"]), b"+OK\r\n");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let after = cmd(&mut c, &[b"VIEW.QUERY", b"v_top", b"LIMIT", b"10"]);
    assert_eq!(before, after, "rebuild is answer-preserving");
    // LIST + DROP
    let r = cmd(&mut c, &[b"VIEW.LIST"]);
    assert!(String::from_utf8_lossy(&r).contains("v_top"));
    assert_eq!(cmd(&mut c, &[b"VIEW.DROP", b"v_top"]), b":1\r\n");
    let r = cmd(&mut c, &[b"VIEW.QUERY", b"v_top"]);
    assert!(r.starts_with(b"-ERR no such view"));
}

#[test]
fn via_hydration_two_phase() {
    let (srv, _gate) = Server::start();
    let mut c = srv.connect();
    // rows: task:<n> with owner field; owner profiles: user:<n>
    for i in 0..12 {
        cmd(
            &mut c,
            &[b"HSET", format!("task:{i}").as_bytes(), b"due", format!("{i}").as_bytes()],
        );
        cmd(
            &mut c,
            &[b"HSET", format!("user:{i}").as_bytes(), b"name", format!("owner-{i}").as_bytes()],
        );
    }
    cmd(&mut c, &[b"IDX.CREATE", b"t_due", b"ON", b"PREFIX", b"task:", b"FIELD", b"due", b"TYPE", b"i64", b"KIND", b"range"]);
    // view over due tasks, VIA maps task:<n> → user:<n>
    let r = cmd(
        &mut c,
        &[b"VIEW.CREATE", b"v_due", b"QUERY", b"t_due", b"RANGE", b"0", b"5",
          b"ORDER", b"BY", b"t_due", b"VIA", b"user:{key.1}"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));
    let r = ready(&mut c, &[b"VIEW.QUERY", b"v_due", b"LIMIT", b"10", b"FIELDS", b"name"]);
    let s = String::from_utf8_lossy(&r);
    assert_eq!(s.matches("task:").count(), 6, "{s}");
    assert!(s.contains("owner-0") && s.contains("owner-5"), "hydrated via template: {s}");
    // missing target = nil, row still present
    cmd(&mut c, &[b"DEL", b"user:3"]);
    let r = cmd(&mut c, &[b"VIEW.QUERY", b"v_due", b"LIMIT", b"10", b"FIELDS", b"name"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("task:3"), "row present: {s}");
    assert!(!s.contains("owner-3"), "hydration nil for deleted target: {s}");
    // FIELDS without VIA errors
    cmd(&mut c, &[b"VIEW.CREATE", b"v_novia", b"QUERY", b"t_due", b"EQ", b"1", b"ORDER", b"BY", b"t_due"]);
    let r = cmd(&mut c, &[b"VIEW.QUERY", b"v_novia", b"FIELDS", b"name"]);
    assert!(String::from_utf8_lossy(&r).contains("requires the view to declare VIA"), "{:?}", String::from_utf8_lossy(&r));
}
