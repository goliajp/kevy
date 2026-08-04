//! R4b-a e2e — the auto-declaration observation loop against a real
//! in-process 8-shard server: refused queries land in the advise log,
//! `IDX.ADVISE` renders each family as the declaration that would
//! serve it, and applying those declarations makes every refused
//! query serve — and clears the log.

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
            "kevy-advisee2e-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(8))
                .bind([127, 0, 0, 1], port)
                .shards(8)
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

/// Every bulk string in a RESP reply, in wire order.
fn bulks(reply: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < reply.len() {
        if reply[i] == b'$' {
            let nl = reply[i..].windows(2).position(|w| w == b"\r\n").unwrap() + i;
            if let Ok(n) = std::str::from_utf8(&reply[i + 1..nl]).unwrap().parse::<i64>()
                && n >= 0
            {
                let start = nl + 2;
                out.push(reply[start..start + n as usize].to_vec());
                i = start + n as usize + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn wait_ready(s: &mut std::net::TcpStream, probe: &[&[u8]]) -> Vec<u8> {
    for _ in 0..200 {
        let reply = cmd(s, probe);
        if !reply.starts_with(b"-INDEXBUILDING") {
            return reply;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("index never left BUILDING for {probe:?}");
}

// ---- the under-declared "user" table -----------------------------------

/// Columns only, plus one index WITHOUT stored values — every refusal
/// shape below is one missing declaration away from serving.
const DECLARE_LEAN: &[&[u8]] = &[
    b"TABLE.DECLARE", b"user", b"PREFIX", b"u:", b"PK", b"id",
    b"COLUMN", b"id", b"str", b"COLUMN", b"name", b"str", b"COLUMN", b"age", b"i64",
    b"COLUMN", b"city", b"str", b"COLUMN", b"bio", b"str",
    b"INDEX", b"age", b"RANGE",
];

/// The lean declaration plus everything IDX.ADVISE asked for.
const REPLACE_FULL: &[&[u8]] = &[
    b"TABLE.REPLACE", b"user", b"PREFIX", b"u:", b"PK", b"id",
    b"COLUMN", b"id", b"str", b"COLUMN", b"name", b"str", b"COLUMN", b"age", b"i64",
    b"COLUMN", b"city", b"str", b"COLUMN", b"bio", b"str",
    b"INDEX", b"age", b"RANGE", b"VALUES", b"name",
    b"INDEX", b"city", b"RANGE",
    b"ORDERPATH", b"by_city", b"ON", b"city", b"THEN", b"age",
];

fn write_rows(s: &mut std::net::TcpStream) {
    for (key, name, age, city, bio) in [
        ("u:1", "alice", "30", "tokyo", "hello world"),
        ("u:2", "bob", "45", "osaka", "goodbye world"),
    ] {
        let reply = cmd(s, &[
            b"HSET", key.as_bytes(), b"id", &key.as_bytes()[2..], b"name", name.as_bytes(),
            b"age", age.as_bytes(), b"city", city.as_bytes(), b"bio", bio.as_bytes(),
        ]);
        assert!(reply.starts_with(b":"), "HSET {key}: {}", String::from_utf8_lossy(&reply));
    }
}

#[test]
fn refusals_become_declarations_that_serve() {
    let srv = Server::start();
    let mut c = srv.connect();
    assert_eq!(cmd(&mut c, DECLARE_LEAN), b"+OK\r\n", "TABLE.DECLARE");
    write_rows(&mut c);
    wait_ready(&mut c, &[b"IDX.QUERY", b"user.age", b"EQ", b"0"]);

    // A fresh catalog install leaves the advise log empty.
    assert_eq!(cmd(&mut c, &[b"IDX.ADVISE"]), b"*0\r\n", "ADVISE after install");

    // Four refusal shapes; the Range family twice, so it ranks first.
    let refused: &[&[&[u8]]] = &[
        &[b"IDX.QUERY", b"user.city", b"RANGE", b"a", b"z"],
        &[b"IDX.QUERY", b"user.city", b"EQ", b"tokyo"],
        &[b"IDX.QUERY", b"user.by_city", b"WHERE", b"city", b"EQ", b"tokyo", b"RANGE", b"age", b"20", b"40"],
        &[b"IDX.QUERY", b"user.bio", b"MATCH", b"hello"],
        &[b"IDX.QUERY", b"user.age", b"RANGE", b"0", b"100", b"FILTER", b"name", b"EQ", b"alice"],
    ];
    for q in refused {
        let reply = cmd(&mut c, q);
        assert!(reply.starts_with(b"-ERR"), "{q:?}: {}", String::from_utf8_lossy(&reply));
    }

    // Most-refused first; equal counts tie-break by name. Each row is
    // [count, name, advice] — bulks() sees the name/advice pairs.
    let expect: &[(&str, &str)] = &[
        ("user.city", "TABLE.DECLARE user … INDEX city range  (column type str)"),
        ("user.age", "add VALUES name (type str) to the user.age declaration"),
        ("user.bio", "IDX.CREATE user.bio ON PREFIX u: FIELD bio TYPE str KIND text"),
        ("user.by_city", "TABLE.DECLARE user … ORDERPATH by_city ON city THEN age"),
    ];
    let adv = bulks(&cmd(&mut c, &[b"IDX.ADVISE"]));
    let want: Vec<Vec<u8>> = expect
        .iter()
        .flat_map(|(n, a)| [n.as_bytes().to_vec(), a.as_bytes().to_vec()])
        .collect();
    assert_eq!(adv, want, "IDX.ADVISE families");

    // A family the catalog cannot ground (unknown column) is refused
    // as usual but WITHHELD from the advice — malformed, not
    // under-declared.
    let reply = cmd(&mut c, &[b"IDX.QUERY", b"user.nosuch", b"RANGE", b"0", b"1"]);
    assert!(reply.starts_with(b"-ERR"), "{}", String::from_utf8_lossy(&reply));
    assert_eq!(bulks(&cmd(&mut c, &[b"IDX.ADVISE"])), want, "ungrounded family withheld");

    // Apply exactly what the advice asked for.
    assert_eq!(cmd(&mut c, REPLACE_FULL), b"+OK\r\n", "TABLE.REPLACE");
    assert_eq!(
        cmd(&mut c, &[
            b"IDX.CREATE", b"user.bio", b"ON", b"PREFIX", b"u:", b"FIELD", b"bio",
            b"TYPE", b"str", b"KIND", b"text",
        ]),
        b"+OK\r\n",
        "IDX.CREATE user.bio"
    );

    // Installing a catalog clears the slate.
    assert_eq!(cmd(&mut c, &[b"IDX.ADVISE"]), b"*0\r\n", "ADVISE after declarations");

    // Every refused query now serves.
    for q in refused {
        let reply = wait_ready(&mut c, q);
        assert!(!reply.starts_with(b"-"), "{q:?}: {}", String::from_utf8_lossy(&reply));
    }
}
