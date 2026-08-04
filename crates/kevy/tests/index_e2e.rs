//! IDX.* — end-to-end against a real 8-shard reactor: rows land
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
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(8)).bind([127, 0, 0, 1], port).shards(8)
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

#[test]
fn hydration_and_compose() {
    let srv = Server::start();
    let mut c = srv.connect();
    for i in 0..20 {
        cmd(
            &mut c,
            &[
                b"HSET", format!("emp:{i}").as_bytes(),
                b"age", format!("{}", 20 + i).as_bytes(),
                b"dept", if i % 2 == 0 { b"eng" } else { b"ops" },
                b"name", format!("emp-{i}").as_bytes(),
            ],
        );
    }
    cmd(&mut c, &[b"IDX.CREATE", b"e_age", b"ON", b"PREFIX", b"emp:", b"FIELD", b"age", b"TYPE", b"i64", b"KIND", b"range"]);
    cmd(&mut c, &[b"IDX.CREATE", b"e_dept", b"ON", b"PREFIX", b"emp:", b"FIELD", b"dept", b"TYPE", b"str", b"KIND", b"range"]);

    // FIELDS hydration: rows come back with name+dept from the owner shard.
    let r = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"e_age", b"RANGE", b"20", b"24", b"LIMIT", b"100", b"FIELDS", b"name", b"dept"],
    );
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("emp-0"), "hydrated name: {s}");
    assert!(s.contains("eng") && s.contains("ops"), "hydrated dept: {s}");
    assert_eq!(s.matches("name\r").count(), 5, "5 rows × field labels: {s}");

    // COMPOSE AND: age in [20,29] AND dept == eng → emp:0,2,4,6,8.
    let r = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"COMPOSE", b"AND", b"e_age", b"RANGE", b"20", b"29", b"e_dept", b"EQ", b"eng", b"LIMIT", b"100"],
    );
    let s = String::from_utf8_lossy(&r);
    assert_eq!(s.matches("emp:").count(), 5, "{s}");
    assert!(s.contains("emp:0") && s.contains("emp:8") && !s.contains("emp:1\r"), "{s}");

    // COMPOSE OR: age in [20,21] OR dept == eng → 0..=1 ∪ evens = 11 keys.
    let r = cmd(
        &mut c,
        &[b"IDX.QUERY", b"COMPOSE", b"OR", b"e_age", b"RANGE", b"20", b"21", b"e_dept", b"EQ", b"eng", b"LIMIT", b"100"],
    );
    let s = String::from_utf8_lossy(&r);
    assert_eq!(s.matches("emp:").count(), 11, "{s}");

    // COMPOSE cursor pagination (key-ordered, no overlap).
    let r1 = cmd(
        &mut c,
        &[b"IDX.QUERY", b"COMPOSE", b"OR", b"e_age", b"RANGE", b"20", b"21", b"e_dept", b"EQ", b"eng", b"LIMIT", b"4"],
    );
    let s1 = String::from_utf8_lossy(&r1);
    let cursor = s1.lines().nth(2).unwrap().to_string();
    assert_ne!(cursor, "0");
    let r2 = cmd(
        &mut c,
        &[b"IDX.QUERY", b"COMPOSE", b"OR", b"e_age", b"RANGE", b"20", b"21", b"e_dept", b"EQ", b"eng", b"LIMIT", b"100", b"CURSOR", cursor.as_bytes()],
    );
    let s2 = String::from_utf8_lossy(&r2);
    assert_eq!(s1.matches("emp:").count() + s2.matches("emp:").count(), 11, "{s1} // {s2}");
    for key in s1.lines().filter(|l| l.starts_with("emp:")) {
        assert!(!s2.contains(&format!("{key}\r")), "no overlap on {key}");
    }

    // COMPOSE with hydration.
    let r = cmd(
        &mut c,
        &[b"IDX.QUERY", b"COMPOSE", b"AND", b"e_age", b"RANGE", b"20", b"23", b"e_dept", b"EQ", b"eng", b"LIMIT", b"10", b"FIELDS", b"name"],
    );
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("emp-0") && s.contains("emp-2"), "{s}");
}

#[test]
fn maxmem_budget_fails_declaratively() {
    let srv = Server::start();
    let mut c = srv.connect();
    for i in 0..200 {
        cmd(&mut c, &[b"HSET", format!("big:{i}").as_bytes(), b"v", format!("{i}").as_bytes()]);
    }
    // 64-byte budget cannot hold 200 entries → FailedOverBudget.
    cmd(
        &mut c,
        &[b"IDX.CREATE", b"tiny", b"ON", b"PREFIX", b"big:", b"FIELD", b"v", b"TYPE", b"i64", b"KIND", b"range", b"MAXMEM", b"64"],
    );
    let mut over = false;
    for _ in 0..100 {
        let r = cmd(&mut c, &[b"IDX.QUERY", b"tiny", b"RANGE", b"0", b"300"]);
        if r.starts_with(b"-INDEXOVERBUDGET") {
            over = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(over, "budget breach surfaces declaratively");
}

#[test]
fn text_kind_match_bm25() {
    let srv = Server::start();
    let mut c = srv.connect();
    let docs: &[(&str, &str)] = &[
        ("doc:1", "Rust is a systems programming language"),
        ("doc:2", "kevy is a pure Rust key value store with full text search"),
        ("doc:3", "全文检索引擎支持中文分词"),
        ("doc:4", "The quick brown fox jumps over the lazy dog"),
        ("doc:5", "検索エンジンを実装する"),
    ];
    for (k, body) in docs {
        cmd(&mut c, &[b"HSET", k.as_bytes(), b"body", body.as_bytes()]);
    }
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"d_body", b"ON", b"PREFIX", b"doc:", b"FIELD", b"body", b"TYPE", b"str", b"KIND", b"text"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));

    // Latin match, ranked: doc:2 mentions rust AND search-adjacent terms.
    let r = query_ready(&mut c, &[b"IDX.QUERY", b"d_body", b"MATCH", b"rust text search", b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("doc:1") && s.contains("doc:2"), "{s}");
    assert!(s.find("doc:2\r").unwrap() < s.find("doc:1\r").unwrap(), "doc:2 ranks first: {s}");
    assert!(!s.contains("doc:4"), "unrelated doc absent: {s}");

    // CJK bigram match.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"d_body", b"MATCH", "中文分词".as_bytes(), b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("doc:3"), "{s}");
    let r = cmd(&mut c, &[b"IDX.QUERY", b"d_body", b"MATCH", "検索".as_bytes(), b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("doc:5") && !s.contains("doc:3"), "{s}");

    // live update re-indexes; delete drops.
    cmd(&mut c, &[b"HSET", b"doc:4", b"body", b"now about rust too"]);
    let r = cmd(&mut c, &[b"IDX.QUERY", b"d_body", b"MATCH", b"rust", b"LIMIT", b"10"]);
    assert!(String::from_utf8_lossy(&r).contains("doc:4"));
    cmd(&mut c, &[b"DEL", b"doc:1"]);
    let r = cmd(&mut c, &[b"IDX.QUERY", b"d_body", b"MATCH", b"systems", b"LIMIT", b"10"]);
    assert!(!String::from_utf8_lossy(&r).contains("doc:1"));

    // hydration + VERIFY stats + LIST.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"d_body", b"MATCH", b"rust", b"LIMIT", b"5", b"FIELDS", b"body"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("key value store"), "hydrated body: {s}");
    let r = cmd(&mut c, &[b"IDX.VERIFY", b"d_body"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("entries\r\n$1\r\n4"), "4 live docs: {s}");
}

#[test]
fn ann_kind_knn_e2e() {
    let srv = Server::start();
    let mut c = srv.connect();
    // 200 points on a 3-d helix; f32 LE blobs
    let blob = |x: f32, y: f32, z: f32| -> Vec<u8> {
        let mut b = Vec::new();
        for v in [x, y, z] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    };
    for i in 0..200 {
        let t = i as f32 * 0.1;
        let mut argv: Vec<&[u8]> = vec![b"HSET"];
        let key = format!("e:{i}");
        argv.push(key.as_bytes());
        argv.push(b"v");
        let b = blob(t.cos(), t.sin(), t * 0.05);
        argv.push(&b);
        cmd(&mut c, &argv);
    }
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"e_v", b"ON", b"PREFIX", b"e:", b"FIELD", b"v",
          b"TYPE", b"vector", b"KIND", b"ann", b"DIM", b"3", b"DISTANCE", b"l2"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));
    // query near point i=100 (t=10.0)
    let t = 10.0f32;
    let q = blob(t.cos(), t.sin(), t * 0.05);
    let mut argv: Vec<&[u8]> = vec![b"IDX.QUERY", b"e_v", b"KNN"];
    argv.push(&q);
    argv.extend_from_slice(&[b"LIMIT", b"3"]);
    let r = query_ready(&mut c, &argv);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("e:100"), "nearest is the exact point: {s}");
    assert!(s.find("e:100\r").unwrap() < s.find("e:99\r").map_or(usize::MAX, |x| x), "{s}");

    // csv debug form + hydration
    let mut argv: Vec<&[u8]> = vec![b"IDX.QUERY", b"e_v", b"KNN"];
    let qs = format!("csv:{},{},{}", t.cos(), t.sin(), t * 0.05);
    argv.push(qs.as_bytes());
    argv.extend_from_slice(&[b"LIMIT", b"1", b"FIELDS", b"v"]);
    let r = cmd(&mut c, &argv);
    assert!(String::from_utf8_lossy(&r).contains("e:100"));

    // update moves a point; delete removes it
    let far = blob(50.0, 50.0, 50.0);
    let mut argv: Vec<&[u8]> = vec![b"HSET", b"e:0", b"v"];
    argv.push(&far);
    cmd(&mut c, &argv);
    let mut argv: Vec<&[u8]> = vec![b"IDX.QUERY", b"e_v", b"KNN"];
    argv.push(&far);
    argv.extend_from_slice(&[b"LIMIT", b"1"]);
    let r = cmd(&mut c, &argv);
    assert!(String::from_utf8_lossy(&r).contains("e:0"), "moved point found at new spot");
    cmd(&mut c, &[b"DEL", b"e:0"]);
    let r = cmd(&mut c, &argv);
    assert!(!String::from_utf8_lossy(&r).contains("e:0\r"), "deleted point gone");

    // VERIFY reports vectors + tombstones; REBUILD compacts
    let r = cmd(&mut c, &[b"IDX.VERIFY", b"e_v"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("entries\r\n$3\r\n199"), "199 living: {s}");
    assert_eq!(cmd(&mut c, &[b"IDX.REBUILD", b"e_v"]), b"+OK\r\n");
    let r = cmd(&mut c, &argv);
    assert!(!String::from_utf8_lossy(&r).contains("e:0\r"), "post-rebuild consistent");

    // dim-mismatch query rejected
    let r = cmd(&mut c, &[b"IDX.QUERY", b"e_v", b"KNN", b"csv:1,2", b"LIMIT", b"1"]);
    assert!(String::from_utf8_lossy(&r).contains("ERR"), "{:?}", String::from_utf8_lossy(&r));
}

#[test]
fn prefix_digest_server_matches_embedded() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"SET", b"pd:str", b"hello"]);
    cmd(&mut c, &[b"HSET", b"pd:hash", b"b", b"2", b"a", b"1"]);
    cmd(&mut c, &[b"RPUSH", b"pd:list", b"x", b"y"]);
    cmd(&mut c, &[b"SADD", b"pd:set", b"q", b"p"]);
    cmd(&mut c, &[b"ZADD", b"pd:zset", b"2", b"n", b"1", b"m"]);
    let r = cmd(&mut c, &[b"PREFIX.DIGEST", b"pd:"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*2\r\n:5\r\n"), "count 5: {s}");
    // must equal the embedded digest of identical data (cross-surface pin)
    let store = kevy_embedded::Store::open(
        kevy_embedded::Config::default().with_ttl_reaper_manual(),
    )
    .unwrap();
    store.set(b"pd:str", b"hello").unwrap();
    store.hset(b"pd:hash", &[(b"a", b"1"), (b"b", b"2")]).unwrap();
    store.rpush(b"pd:list", &[b"x", b"y"]).unwrap();
    store.sadd(b"pd:set", &[b"p", b"q"]).unwrap();
    store.zadd(b"pd:zset", &[(1.0, b"m" as &[u8]), (2.0, b"n")]).unwrap();
    let (n, d) = store.prefix_digest(b"pd:");
    assert_eq!(n, 5);
    assert!(s.contains(&format!("{d:016x}")), "server {s} vs embedded {d:016x}");
}

#[test]
fn agg_kind_group_by_e2e() {
    let srv = Server::start();
    let mut c = srv.connect();
    // orders: status group, amount value
    for (i, (st, amt)) in [("paid", 100), ("paid", 250), ("open", 40), ("paid", 100),
                            ("open", 999), ("void", 7)].iter().enumerate() {
        cmd(&mut c, &[b"HSET", format!("ord:{i}").as_bytes(), b"status", st.as_bytes(),
                       b"amount", amt.to_string().as_bytes()]);
    }
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"ord_amt", b"ON", b"PREFIX", b"ord:", b"FIELD", b"amount",
          b"TYPE", b"i64", b"KIND", b"agg", b"GROUPBY", b"status"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));
    // single group stats
    let r = query_ready(&mut c, &[b"IDX.QUERY", b"ord_amt", b"GROUP", b"paid"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*5\r\n$1\r\n3\r\n$3\r\n450\r\n"), "count 3 sum 450: {s}");
    assert!(s.contains("100") && s.contains("250") && s.contains("150"), "min/max/avg: {s}");
    // unknown group = count 0, nils
    let r = cmd(&mut c, &[b"IDX.QUERY", b"ord_amt", b"GROUP", b"nope"]);
    assert!(String::from_utf8_lossy(&r).starts_with("*5\r\n$1\r\n0\r\n"), "{:?}", String::from_utf8_lossy(&r));
    // GROUPS ranked by sum: open (1039) > paid (450) > void (7)
    let r = cmd(&mut c, &[b"IDX.QUERY", b"ord_amt", b"GROUPS", b"BY", b"sum", b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    let (po, pp, pv) = (s.find("open").unwrap(), s.find("paid").unwrap(), s.find("void").unwrap());
    assert!(po < pp && pp < pv, "sum ranking: {s}");
    // live maintenance: pay the open orders → open group drains
    cmd(&mut c, &[b"HSET", b"ord:2", b"status", b"paid"]);
    cmd(&mut c, &[b"HSET", b"ord:4", b"status", b"paid"]);
    let r = cmd(&mut c, &[b"IDX.QUERY", b"ord_amt", b"GROUP", b"open"]);
    assert!(String::from_utf8_lossy(&r).starts_with("*5\r\n$1\r\n0\r\n"));
    let r = cmd(&mut c, &[b"IDX.QUERY", b"ord_amt", b"GROUP", b"paid"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*5\r\n$1\r\n5\r\n"), "5 paid rows now: {s}");
    assert!(s.contains("999"), "new max from regrouped row: {s}");
    // delete removes from its group; min recomputes exactly
    cmd(&mut c, &[b"DEL", b"ord:2"]);
    let r = cmd(&mut c, &[b"IDX.QUERY", b"ord_amt", b"GROUP", b"paid"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*5\r\n$1\r\n4\r\n"), "{s}");
    // exclusion counted: a row missing the amount field
    cmd(&mut c, &[b"HSET", b"ord:9", b"status", b"paid"]);
    let r = cmd(&mut c, &[b"IDX.VERIFY", b"ord_amt"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("coerce_failures\r\n$1\r\n1") || s.contains("$1\r\n1"), "excluded visible: {s}");
    // bad CREATEs rejected
    let r = cmd(&mut c, &[b"IDX.CREATE", b"bad1", b"ON", b"PREFIX", b"x:", b"FIELD", b"f",
                           b"TYPE", b"i64", b"KIND", b"agg"]);
    assert!(String::from_utf8_lossy(&r).contains("GROUPBY"), "{:?}", String::from_utf8_lossy(&r));
    let r = cmd(&mut c, &[b"IDX.CREATE", b"bad2", b"ON", b"PREFIX", b"x:", b"FIELD", b"f",
                           b"TYPE", b"str", b"KIND", b"agg", b"GROUPBY", b"g"]);
    assert!(String::from_utf8_lossy(&r).contains("i64|f64"), "{:?}", String::from_utf8_lossy(&r));
}

/// `FIELDS a b WEIGHTS …` over the wire — the multi-attribute path that
/// was declarable through the embedded API but had no IDX.CREATE syntax.
/// The single-`FIELD` tests above are the byte-identical regression for
/// the parser change that made this possible.
#[test]
fn fields_multi_attribute_create_and_rank() {
    let srv = Server::start();
    let mut c = srv.connect();
    // Two docs. In post:1 the term is only in the (heavily weighted)
    // title; in post:2 it is only in the body. The weight must make
    // post:1 rank first — the comparability a single-field index cannot
    // give, since it would score the two fields on separate corpora.
    cmd(&mut c, &[b"HSET", b"post:1", b"title", b"rust", b"body", b"a long body about other things entirely"]);
    cmd(&mut c, &[b"HSET", b"post:2", b"title", b"unrelated", b"body", b"this body mentions rust once among much filler text"]);
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"posts", b"ON", b"PREFIX", b"post:", b"FIELDS", b"title", b"body",
          b"WEIGHTS", b"5", b"1", b"TYPE", b"str", b"KIND", b"text"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));

    let r = query_ready(&mut c, &[b"IDX.QUERY", b"posts", b"MATCH", b"rust", b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("post:1") && s.contains("post:2"), "both match: {s}");
    assert!(
        s.find("post:1\r").unwrap() < s.find("post:2\r").unwrap(),
        "the weight-5 title hit must outrank the weight-1 body hit: {s}"
    );
}

/// FIELDS without weights defaults every field to 1.0, and a
/// multi-field non-text index is refused by the catalog.
#[test]
fn fields_defaults_and_non_text_refusal() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"HSET", b"d:1", b"a", b"alpha", b"b", b"beta"]);
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"unweighted", b"ON", b"PREFIX", b"d:", b"FIELDS", b"a", b"b",
          b"TYPE", b"str", b"KIND", b"text"],
    );
    assert_eq!(r, b"+OK\r\n", "unweighted FIELDS defaults to 1.0: {:?}", String::from_utf8_lossy(&r));

    // A range index reads one scalar; two fields must be refused.
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"badrange", b"ON", b"PREFIX", b"d:", b"FIELDS", b"a", b"b",
          b"TYPE", b"i64", b"KIND", b"range"],
    );
    assert!(String::from_utf8_lossy(&r).starts_with("-ERR"), "range refuses two fields: {:?}", String::from_utf8_lossy(&r));
}

/// Mismatched WEIGHTS/FIELDS counts and empty FIELDS are usage errors,
/// not silent truncation.
#[test]
fn fields_arity_errors() {
    let srv = Server::start();
    let mut c = srv.connect();
    let bad: &[&[&[u8]]] = &[
        &[b"IDX.CREATE", b"x", b"ON", b"PREFIX", b"p:", b"FIELDS", b"a", b"b",
          b"WEIGHTS", b"1", b"TYPE", b"str", b"KIND", b"text"], // 2 fields, 1 weight
        &[b"IDX.CREATE", b"x", b"ON", b"PREFIX", b"p:", b"FIELDS",
          b"TYPE", b"str", b"KIND", b"text"], // no field names
    ];
    for parts in bad {
        let r = cmd(&mut c, parts);
        assert!(
            String::from_utf8_lossy(&r).starts_with("-ERR"),
            "must be a usage error: {:?} -> {:?}",
            parts.len(),
            String::from_utf8_lossy(&r)
        );
    }
}

/// `WITH POSITIONS` creates a text index that records token positions.
/// The flag does not change ranking — that is what the phrase query
/// (step 5d) uses; an ordinary MATCH ranks exactly as without it — so
/// here the wire path is verified end-to-end: create succeeds, the index
/// backfills, and a plain MATCH returns the expected hits.
#[test]
fn positions_create_succeeds_and_ranking_is_unaffected() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"HSET", b"doc:1", b"body", b"the quick brown fox"]);
    cmd(&mut c, &[b"HSET", b"doc:2", b"body", b"quick systems programming"]);
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"withpos", b"ON", b"PREFIX", b"doc:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text", b"WITH", b"POSITIONS"],
    );
    assert_eq!(r, b"+OK\r\n", "WITH POSITIONS creates: {:?}", String::from_utf8_lossy(&r));
    let r = query_ready(&mut c, &[b"IDX.QUERY", b"withpos", b"MATCH", b"quick", b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("doc:1") && s.contains("doc:2"), "both docs match 'quick': {s}");
}

/// `WITH POSITIONS` is text-only, and `WITH` accepts only `POSITIONS` —
/// both are usage errors, not silently accepted-and-ignored.
#[test]
fn positions_flag_is_text_only_and_validated() {
    let srv = Server::start();
    let mut c = srv.connect();
    // A range index reads one scalar and maintains no positional
    // side-channel, so the flag must be refused.
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"rangepos", b"ON", b"PREFIX", b"n:", b"FIELD", b"age",
          b"TYPE", b"i64", b"KIND", b"range", b"WITH", b"POSITIONS"],
    );
    assert!(
        String::from_utf8_lossy(&r).starts_with("-ERR"),
        "a range index refuses WITH POSITIONS: {:?}",
        String::from_utf8_lossy(&r)
    );
    // WITH is a keyword flag that only spells POSITIONS.
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"junkwith", b"ON", b"PREFIX", b"t:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text", b"WITH", b"NONSENSE"],
    );
    assert!(
        String::from_utf8_lossy(&r).starts_with("-ERR"),
        "WITH NONSENSE is a usage error: {:?}",
        String::from_utf8_lossy(&r)
    );
}

/// A quoted phrase in the MATCH text matches only documents whose terms
/// are adjacent and in order — the cross-shard payoff of the positional
/// side-channel. Docs land on different shards, so this also exercises
/// the two-pass fan-out carrying the quoted text verbatim to pass 2.
#[test]
fn phrase_query_matches_only_adjacent_docs() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"HSET", b"p:1", b"body", b"the quick brown fox jumps"]);
    cmd(&mut c, &[b"HSET", b"p:2", b"body", b"quick red then a brown hare"]);
    cmd(&mut c, &[b"HSET", b"p:3", b"body", b"a brown quick animal appears"]);
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"ph", b"ON", b"PREFIX", b"p:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text", b"WITH", b"POSITIONS"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));
    // The MATCH text is the single quoted argument `"quick brown"`.
    let r = query_ready(&mut c, &[b"IDX.QUERY", b"ph", b"MATCH", b"\"quick brown\"", b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("p:1"), "adjacent, in-order phrase matches p:1: {s}");
    assert!(!s.contains("p:2"), "far-apart terms are not a phrase: {s}");
    assert!(!s.contains("p:3"), "reversed order is not the phrase: {s}");
    // A plain term query still ORs — every doc has "brown".
    let r = query_ready(&mut c, &[b"IDX.QUERY", b"ph", b"MATCH", b"brown", b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("p:1") && s.contains("p:2") && s.contains("p:3"), "term OR matches all: {s}");
}

/// HIGHLIGHT adds a trailing per-hit element naming each field and the
/// byte spans that matched — here the phrase "quick brown" in the body.
/// Without HIGHLIGHT the row keeps its `[key, score]` shape.
#[test]
fn highlight_returns_match_spans() {
    let srv = Server::start();
    let mut c = srv.connect();
    // "the quick brown fox": quick at bytes 4..9, brown at 10..15.
    cmd(&mut c, &[b"HSET", b"h:1", b"body", b"the quick brown fox"]);
    // WITH POSITIONS so the phrase query matches; highlighting itself
    // re-analyses the text and needs no positions, but the phrase *query*
    // does.
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"hi", b"ON", b"PREFIX", b"h:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text", b"WITH", b"POSITIONS"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));

    // Baseline: no HIGHLIGHT → no field name, no spans in the reply.
    let base = query_ready(&mut c, &[b"IDX.QUERY", b"hi", b"MATCH", b"quick", b"LIMIT", b"5"]);
    let bs = String::from_utf8_lossy(&base);
    assert!(bs.contains("h:1"), "matches: {bs}");
    assert!(!bs.contains("body"), "no highlights requested → field name absent: {bs}");

    // HIGHLIGHT the phrase: the reply carries `body` and the two spans.
    let r = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"hi", b"MATCH", b"\"quick brown\"", b"LIMIT", b"5", b"HIGHLIGHT"],
    );
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("h:1"), "phrase matches: {s}");
    assert!(s.contains("body"), "highlights name the field: {s}");
    // quick 4..9 and brown 10..15 appear as bulk-string offsets.
    for n in ["4", "9", "10", "15"] {
        assert!(s.contains(&format!("${}\r\n{n}\r\n", n.len())), "span offset {n} present: {s}");
    }
}

/// A `word*` prefix in the MATCH text matches every term sharing the
/// prefix — search-as-you-type — over the real fan-out, with the
/// expansion terms' df aggregated globally (pass 1 expands per shard).
#[test]
fn prefix_query_over_the_wire() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"HSET", b"p:1", b"body", b"quick fox"]);
    cmd(&mut c, &[b"HSET", b"p:2", b"body", b"quiet night"]);
    cmd(&mut c, &[b"HSET", b"p:3", b"body", b"slow turtle"]);
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"pf", b"ON", b"PREFIX", b"p:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));
    let r = query_ready(&mut c, &[b"IDX.QUERY", b"pf", b"MATCH", b"qui*", b"LIMIT", b"10"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("p:1") && s.contains("p:2"), "qui* matches quick and quiet: {s}");
    assert!(!s.contains("p:3"), "slow has no qui- term: {s}");
    // A plain term query is unchanged.
    let r = query_ready(&mut c, &[b"IDX.QUERY", b"pf", b"MATCH", b"quick", b"LIMIT", b"10"]);
    assert!(String::from_utf8_lossy(&r).contains("p:1"), "plain term still works");
}

/// `TYPO n` fuzzes each bare term by up to n edits — a misspelling still
/// finds its document over the real fan-out, and the budget is a bound.
#[test]
fn typo_query_over_the_wire() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"HSET", b"t:1", b"body", b"quick brown fox"]);
    cmd(&mut c, &[b"HSET", b"t:2", b"body", b"slow green turtle"]);
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"tp", b"ON", b"PREFIX", b"t:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));
    // "quik" is one edit from "quick": exact finds nothing, TYPO 1 finds it.
    let exact = query_ready(&mut c, &[b"IDX.QUERY", b"tp", b"MATCH", b"quik", b"LIMIT", b"10"]);
    assert!(!String::from_utf8_lossy(&exact).contains("t:1"), "exact query misses the typo");
    let fuzzy = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"tp", b"MATCH", b"quik", b"LIMIT", b"10", b"TYPO", b"1"],
    );
    let s = String::from_utf8_lossy(&fuzzy);
    assert!(s.contains("t:1"), "TYPO 1 reaches quick: {s}");
    assert!(!s.contains("t:2"), "turtle is nowhere near: {s}");
    // AUTO is in the frozen surface but not built — a clear error.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"tp", b"MATCH", b"quik", b"TYPO", b"AUTO"]);
    assert!(String::from_utf8_lossy(&r).starts_with("-ERR"), "TYPO AUTO errors clearly");
}

/// `OFFSET n` skips the first n hits of the MERGED ranking, so paging
/// with LIMIT/OFFSET never repeats or drops a row across shards.
#[test]
fn offset_pages_the_merged_ranking() {
    let srv = Server::start();
    let mut c = srv.connect();
    // Five docs all containing "rust", with decreasing term density so
    // the ranking is stable.
    for i in 0..5 {
        let body = format!("rust {}", "filler ".repeat(i));
        cmd(&mut c, &[b"HSET", format!("o:{i}").as_bytes(), b"body", body.as_bytes()]);
    }
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"off", b"ON", b"PREFIX", b"o:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));

    let all = query_ready(&mut c, &[b"IDX.QUERY", b"off", b"MATCH", b"rust", b"LIMIT", b"10"]);
    let all_s = String::from_utf8_lossy(&all);
    assert_eq!(all_s.matches("o:").count(), 5, "all five match: {all_s}");

    // Page 1 (LIMIT 2) and page 2 (LIMIT 2 OFFSET 2) must not overlap and
    // together cover the first four of the merged ranking.
    let p1 = query_ready(&mut c, &[b"IDX.QUERY", b"off", b"MATCH", b"rust", b"LIMIT", b"2"]);
    let p2 = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"off", b"MATCH", b"rust", b"LIMIT", b"2", b"OFFSET", b"2"],
    );
    let (s1, s2) = (String::from_utf8_lossy(&p1), String::from_utf8_lossy(&p2));
    assert_eq!(s1.matches("o:").count(), 2, "page 1 has two rows: {s1}");
    assert_eq!(s2.matches("o:").count(), 2, "page 2 has two rows: {s2}");
    for i in 0..5 {
        let k = format!("o:{i}\r");
        assert!(!(s1.contains(&k) && s2.contains(&k)), "o:{i} appears on both pages");
    }
    // An offset past the end is empty, not an error.
    let past = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"off", b"MATCH", b"rust", b"LIMIT", b"2", b"OFFSET", b"99"],
    );
    assert_eq!(String::from_utf8_lossy(&past).matches("o:").count(), 0, "past the end is empty");
}

/// `IN <field…>` scopes a MATCH to the named fields over the real
/// cross-shard fan-out: the two-pass statistics are gathered over those
/// fields, so it is a field-scoped BM25, and naming a field the index
/// does not declare is an error rather than an empty result.
#[test]
fn field_scope_over_the_wire() {
    let srv = Server::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"HSET", b"f:1", b"title", b"rust engine", b"body", b"a long body about gardening"]);
    cmd(&mut c, &[b"HSET", b"f:2", b"title", b"gardening weekly", b"body", b"this body mentions rust once or twice"]);
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"fs", b"ON", b"PREFIX", b"f:", b"FIELDS", b"title", b"body",
          b"TYPE", b"str", b"KIND", b"text", b"WITH", b"POSITIONS"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));

    // Unscoped: both rows mention rust somewhere.
    let all = query_ready(&mut c, &[b"IDX.QUERY", b"fs", b"MATCH", b"rust", b"LIMIT", b"5"]);
    let s = String::from_utf8_lossy(&all);
    assert!(s.contains("f:1") && s.contains("f:2"), "both match unscoped: {s}");

    // Scoped to the title, only f:1 does.
    let title = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"fs", b"MATCH", b"rust", b"LIMIT", b"5", b"IN", b"title"],
    );
    let s = String::from_utf8_lossy(&title);
    assert!(s.contains("f:1"), "the title match survives: {s}");
    assert!(!s.contains("f:2"), "the body-only row is out of scope: {s}");

    // Scoped to the body, only f:2.
    let body = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"fs", b"MATCH", b"rust", b"LIMIT", b"5", b"IN", b"body"],
    );
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("f:2") && !s.contains("f:1"), "body scope: {s}");

    // Naming both fields is the unscoped query again.
    let both = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"fs", b"MATCH", b"rust", b"LIMIT", b"5", b"IN", b"title", b"body"],
    );
    let s = String::from_utf8_lossy(&both);
    assert!(s.contains("f:1") && s.contains("f:2"), "every field = unscoped: {s}");

    // IN composes with the other clauses.
    let combo = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"fs", b"MATCH", b"rusty", b"LIMIT", b"5", b"IN", b"title", b"TYPO", b"1"],
    );
    let s = String::from_utf8_lossy(&combo);
    assert!(s.contains("f:1") && !s.contains("f:2"), "typo inside the title scope: {s}");

    // An undeclared field errors, and the error says what IS indexed.
    let bad = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"fs", b"MATCH", b"rust", b"LIMIT", b"5", b"IN", b"titel"],
    );
    let s = String::from_utf8_lossy(&bad);
    assert!(s.starts_with("-ERR"), "undeclared field is an error: {s}");
    assert!(s.contains("titel"), "names the offending field: {s}");
    assert!(s.contains("title") && s.contains("body"), "lists the declared fields: {s}");
}

/// `FILTER` restricts which documents can be hits without changing what
/// a term is worth, over the real cross-shard fan-out — and the
/// predicate reaches documents that rank below the unfiltered leaders,
/// which is the whole reason it cannot be applied after the merge.
#[test]
fn filter_over_the_wire() {
    let srv = Server::start();
    let mut c = srv.connect();
    // Ten documents that all match "rust"; the term count falls with the
    // index, and so does the price — so the cheap ones rank worst.
    for i in 0..10u32 {
        let body = format!("rust {}", "rust ".repeat((10 - i) as usize));
        let price = format!("{}", (10 - i) * 10);
        cmd(&mut c, &[b"HSET", format!("p:{i}").as_bytes(), b"body", body.as_bytes(),
                      b"price", price.as_bytes(), b"status", if i % 2 == 0 { b"live" } else { b"draft" }]);
    }
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"pf", b"ON", b"PREFIX", b"p:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text",
          b"VALUES", b"price", b"status", b"TYPES", b"i64", b"str"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));

    // Unfiltered, the top 3 are the priciest — the ones the filter below
    // rejects.
    let plain = query_ready(&mut c, &[b"IDX.QUERY", b"pf", b"MATCH", b"rust", b"LIMIT", b"3"]);
    let s = String::from_utf8_lossy(&plain);
    assert!(s.contains("p:0") && s.contains("p:1"), "unfiltered leaders: {s}");

    // Filtered to the cheap half, the same LIMIT 3 returns documents that
    // rank below those leaders — not an empty page.
    let cheap = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"pf", b"MATCH", b"rust", b"LIMIT", b"3", b"FILTER", b"price", b"RANGE", b"10", b"50"],
    );
    let s = String::from_utf8_lossy(&cheap);
    assert!(!s.contains("p:0") && !s.contains("p:1"), "the pricey leaders are out: {s}");
    let hits = s.matches("p:").count();
    assert_eq!(hits, 3, "the page is filled from further down the ranking: {s}");

    // A numeric range compares as a number, not as text: "9" must not
    // sort above "10".
    let numeric = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"pf", b"MATCH", b"rust", b"LIMIT", b"10", b"FILTER", b"price", b"RANGE", b"10", b"20"],
    );
    let s = String::from_utf8_lossy(&numeric);
    assert_eq!(s.matches("p:").count(), 2, "prices 10 and 20 only: {s}");

    // EQ on a text value, and two predicates ANDing.
    let both = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"pf", b"MATCH", b"rust", b"LIMIT", b"10",
          b"FILTER", b"status", b"EQ", b"live", b"FILTER", b"price", b"RANGE", b"10", b"50"],
    );
    let s = String::from_utf8_lossy(&both);
    assert!(s.contains("p:6") && s.contains("p:8"), "even indexes are live: {s}");
    assert!(!s.contains("p:5") && !s.contains("p:7"), "odd ones are drafts: {s}");

    // A field the index does not store is an error naming what it does.
    let bad = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"pf", b"MATCH", b"rust", b"FILTER", b"colour", b"EQ", b"red"],
    );
    let s = String::from_utf8_lossy(&bad);
    assert!(s.starts_with("-ERR"), "unstored field errors: {s}");
    assert!(s.contains("colour") && s.contains("price") && s.contains("status"), "{s}");

    // A bound that is not of the declared type is an error too, rather
    // than a silently empty page.
    let badbound = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"pf", b"MATCH", b"rust", b"FILTER", b"price", b"RANGE", b"cheap", b"50"],
    );
    let s = String::from_utf8_lossy(&badbound);
    assert!(s.starts_with("-ERR") && s.contains("i64"), "bad bound errors: {s}");
}

/// `SORT` selects by a stored value across the real fan-out: the page is
/// the globally cheapest matching documents, not the best-scoring ones
/// re-ordered. Documents spread over shards, so this only works if each
/// shard picked its page BY the key and the origin merged by it too.
#[test]
fn sort_over_the_wire() {
    let srv = Server::start();
    let mut c = srv.connect();
    for i in 0..10u32 {
        let body = format!("rust {}", "rust ".repeat((10 - i) as usize));
        let price = format!("{}", (10 - i) * 10);
        cmd(&mut c, &[b"HSET", format!("s:{i}").as_bytes(), b"body", body.as_bytes(),
                      b"price", price.as_bytes()]);
    }
    // Two rows that match but carry no price at all.
    cmd(&mut c, &[b"HSET", b"s:x", b"body", b"rust"]);
    cmd(&mut c, &[b"HSET", b"s:y", b"body", b"rust", b"price", b"not a number"]);
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"sf", b"ON", b"PREFIX", b"s:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text", b"VALUES", b"price", b"TYPES", b"i64"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));

    // By score, the top 3 are the priciest (most repeats).
    let plain = query_ready(&mut c, &[b"IDX.QUERY", b"sf", b"MATCH", b"rust", b"LIMIT", b"3"]);
    let s = String::from_utf8_lossy(&plain);
    assert!(s.contains("s:0") && s.contains("s:1") && s.contains("s:2"), "by score: {s}");

    // Ascending by price, the page is the three cheapest — which are the
    // three WORST scorers, so a re-order of the score page would have
    // returned none of them.
    let asc = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"sf", b"MATCH", b"rust", b"LIMIT", b"3", b"SORT", b"price", b"ASC"],
    );
    let s = String::from_utf8_lossy(&asc);
    assert!(s.contains("s:9") && s.contains("s:8") && s.contains("s:7"), "cheapest: {s}");
    assert!(!s.contains("s:0") && !s.contains("s:1"), "the score leaders are not on this page: {s}");

    // Descending is the other end.
    let desc = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"sf", b"MATCH", b"rust", b"LIMIT", b"3", b"SORT", b"price", b"DESC"],
    );
    let s = String::from_utf8_lossy(&desc);
    assert!(s.contains("s:0") && s.contains("s:1") && s.contains("s:2"), "priciest: {s}");

    // Numeric, not lexicographic: ascending must start at 10, not at 100.
    let one = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"sf", b"MATCH", b"rust", b"LIMIT", b"1", b"SORT", b"price", b"ASC"],
    );
    let s = String::from_utf8_lossy(&one);
    assert!(s.contains("s:9"), "10 sorts below 100: {s}");

    // Missing and uncoercible values sort last in BOTH directions.
    for dir in [b"ASC" as &[u8], b"DESC"] {
        let all = query_ready(
            &mut c,
            &[b"IDX.QUERY", b"sf", b"MATCH", b"rust", b"LIMIT", b"12", b"SORT", b"price", dir],
        );
        let s = String::from_utf8_lossy(&all);
        let x = s.find("s:x").expect("s:x present");
        let y = s.find("s:y").expect("s:y present");
        let last_priced = s.rfind("s:9").max(s.rfind("s:0")).expect("a priced row");
        assert!(x > last_priced && y > last_priced, "unknowns last ({:?}): {s}", dir);
    }

    // SORT composes with FILTER, and an unstored field errors.
    let both = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"sf", b"MATCH", b"rust", b"LIMIT", b"2",
          b"FILTER", b"price", b"RANGE", b"50", b"100", b"SORT", b"price", b"ASC"],
    );
    let s = String::from_utf8_lossy(&both);
    assert!(s.contains("s:5") && s.contains("s:4"), "cheapest of the qualifying: {s}");
    let bad = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"sf", b"MATCH", b"rust", b"SORT", b"colour", b"ASC"],
    );
    let s = String::from_utf8_lossy(&bad);
    assert!(s.starts_with("-ERR") && s.contains("price"), "unstored sort field: {s}");
}

/// `DISTINCT` collapses across the whole fan-out: the page holds one
/// document per value, each the best of its group, even when the group's
/// members are spread over different shards.
#[test]
fn distinct_over_the_wire() {
    let srv = Server::start();
    let mut c = srv.connect();
    // Six documents in three price groups; the leaders are not the three
    // top scorers, so collapsing a score page after the fact would return
    // a short page.
    for (k, reps, price) in [
        ("a1", 9, "10"), ("a2", 8, "10"),
        ("b1", 7, "20"), ("b2", 6, "20"),
        ("c1", 5, "30"), ("c2", 4, "30"),
    ] {
        let body = format!("rust {}", "rust ".repeat(reps));
        cmd(&mut c, &[b"HSET", format!("g:{k}").as_bytes(), b"body", body.as_bytes(),
                      b"price", price.as_bytes()]);
    }
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"gf", b"ON", b"PREFIX", b"g:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text", b"VALUES", b"price", b"TYPES", b"i64"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));

    // Plain: the top 3 are a1, a2, b1 — two share a price.
    let plain = query_ready(&mut c, &[b"IDX.QUERY", b"gf", b"MATCH", b"rust", b"LIMIT", b"3"]);
    let s = String::from_utf8_lossy(&plain);
    assert!(s.contains("g:a1") && s.contains("g:a2"), "duplicates ride along: {s}");

    // DISTINCT: one per price, each the best of its group, and the page
    // is still full.
    let d = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"gf", b"MATCH", b"rust", b"LIMIT", b"3", b"DISTINCT", b"price"],
    );
    let s = String::from_utf8_lossy(&d);
    assert_eq!(s.matches("g:").count(), 3, "the page is filled with distinct rows: {s}");
    for k in ["g:a1", "g:b1", "g:c1"] {
        assert!(s.contains(k), "{k} is its group's best: {s}");
    }
    for k in ["g:a2", "g:b2", "g:c2"] {
        assert!(!s.contains(k), "{k} is collapsed away: {s}");
    }

    // Composes with SORT and FILTER.
    let both = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"gf", b"MATCH", b"rust", b"LIMIT", b"5",
          b"DISTINCT", b"price", b"SORT", b"price", b"DESC"],
    );
    let s = String::from_utf8_lossy(&both);
    let c1 = s.find("g:c1").expect("c1 present");
    let a1 = s.find("g:a1").expect("a1 present");
    assert!(c1 < a1, "descending by price puts 30 before 10: {s}");

    // An unstored field errors rather than collapsing nothing.
    let bad = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"gf", b"MATCH", b"rust", b"DISTINCT", b"colour"],
    );
    let s = String::from_utf8_lossy(&bad);
    assert!(s.starts_with("-ERR") && s.contains("price"), "unstored distinct field: {s}");
}

/// `FACET` counts every match across the fan-out, not just the page, and
/// rides back as one trailing element so an unfaceted reply keeps its
/// previous shape.
#[test]
fn facet_over_the_wire() {
    let srv = Server::start();
    let mut c = srv.connect();
    for (k, reps, price) in [
        ("a1", 9, "10"), ("a2", 8, "10"),
        ("b1", 7, "20"), ("b2", 6, "20"),
        ("c1", 5, "30"), ("c2", 4, "30"),
    ] {
        let body = format!("rust {}", "rust ".repeat(reps));
        cmd(&mut c, &[b"HSET", format!("f2:{k}").as_bytes(), b"body", body.as_bytes(),
                      b"price", price.as_bytes()]);
    }
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"ff", b"ON", b"PREFIX", b"f2:", b"FIELD", b"body",
          b"TYPE", b"str", b"KIND", b"text", b"VALUES", b"price", b"TYPES", b"i64"],
    );
    assert_eq!(r, b"+OK\r\n", "{:?}", String::from_utf8_lossy(&r));

    // A LIMIT-1 page still reports every bucket with both documents —
    // the counts come from the match set, not the page.
    let f = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"ff", b"MATCH", b"rust", b"LIMIT", b"1", b"FACET", b"price"],
    );
    let s = String::from_utf8_lossy(&f);
    assert!(s.contains("price"), "the facet field is named: {s}");
    for v in ["10", "20", "30"] {
        assert!(s.contains(&format!("${}\r\n{v}\r\n", v.len())), "bucket {v} present: {s}");
    }
    // Three buckets of two, summed across shards.
    assert_eq!(s.matches("$1\r\n2\r\n").count(), 3, "each bucket counts two: {s}");

    // FILTER restricts the counts; DISTINCT does not.
    let filtered = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"ff", b"MATCH", b"rust", b"LIMIT", b"10",
          b"FILTER", b"price", b"RANGE", b"20", b"30", b"FACET", b"price"],
    );
    let s = String::from_utf8_lossy(&filtered);
    assert!(!s.contains("\r\n10\r\n"), "the excluded price has no bucket: {s}");
    let collapsed = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"ff", b"MATCH", b"rust", b"LIMIT", b"10",
          b"DISTINCT", b"price", b"FACET", b"price"],
    );
    let s = String::from_utf8_lossy(&collapsed);
    assert_eq!(s.matches("$1\r\n2\r\n").count(), 3, "collapsing the page does not change what matched: {s}");

    // Without FACET the reply is exactly the rows, no trailing element.
    let plain = query_ready(&mut c, &[b"IDX.QUERY", b"ff", b"MATCH", b"rust", b"LIMIT", b"2"]);
    let s = String::from_utf8_lossy(&plain);
    assert!(s.starts_with("*2\r\n"), "two rows and nothing else: {s}");

    // An unstored field errors rather than counting nothing.
    let bad = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"ff", b"MATCH", b"rust", b"FACET", b"colour"],
    );
    let s = String::from_utf8_lossy(&bad);
    assert!(s.starts_with("-ERR") && s.contains("price"), "unstored facet field: {s}");
}

/// The scalar-VALUES clause fixture: six rows on `v:` — ages 10..60;
/// v:3 has no city, v:4 no price, v:6 an uncoercible price — spread
/// over the real 8-shard reactor so every clause exercises the
/// extension fan-out and the origin merge.
fn seed_values_rows(c: &mut std::net::TcpStream) {
    let rows: &[(&str, &[(&str, &str)])] = &[
        ("v:1", &[("age", "10"), ("city", "tokyo"), ("price", "5")]),
        ("v:2", &[("age", "20"), ("city", "osaka"), ("price", "3")]),
        ("v:3", &[("age", "30"), ("price", "8")]),
        ("v:4", &[("age", "40"), ("city", "tokyo")]),
        ("v:5", &[("age", "50"), ("city", "kyoto"), ("price", "3")]),
        ("v:6", &[("age", "60"), ("city", "osaka"), ("price", "x")]),
    ];
    for (key, fields) in rows {
        let mut argv: Vec<&[u8]> = vec![b"HSET", key.as_bytes()];
        for (f, v) in *fields {
            argv.push(f.as_bytes());
            argv.push(v.as_bytes());
        }
        cmd(c, &argv);
    }
}

/// The flat `[cursor "0", rows]` reply for keys+values.
fn flat_reply(rows: &[(&str, &str)]) -> Vec<u8> {
    let mut out = String::from("*2\r\n$1\r\n0\r\n");
    out.push_str(&format!("*{}\r\n", rows.len() * 2));
    for (k, v) in rows {
        out.push_str(&format!("${}\r\n{}\r\n", k.len(), k));
        out.push_str(&format!("${}\r\n{}\r\n", v.len(), v));
    }
    out.into_bytes()
}

#[test]
fn scalar_values_filter_sort_distinct_facet_offset() {
    let srv = Server::start();
    let mut c = srv.connect();
    seed_values_rows(&mut c);
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"vals", b"ON", b"PREFIX", b"v:", b"FIELD", b"age", b"TYPE", b"i64",
          b"KIND", b"range", b"VALUES", b"city", b"price", b"TYPES", b"str", b"i64"],
    );
    assert_eq!(r, b"+OK\r\n");
    // A twin without VALUES over the same domain: the plain reply's
    // byte-stability proof (A5 on the wire).
    let r = cmd(
        &mut c,
        &[b"IDX.CREATE", b"plainidx", b"ON", b"PREFIX", b"v:", b"FIELD", b"age", b"TYPE", b"i64",
          b"KIND", b"range"],
    );
    assert_eq!(r, b"+OK\r\n");

    let all = flat_reply(&[
        ("v:1", "10"), ("v:2", "20"), ("v:3", "30"), ("v:4", "40"), ("v:5", "50"), ("v:6", "60"),
    ]);
    let with_values =
        query_ready(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"LIMIT", b"100"]);
    let without = query_ready(
        &mut c,
        &[b"IDX.QUERY", b"plainidx", b"RANGE", b"0", b"100", b"LIMIT", b"100"],
    );
    assert_eq!(with_values, all, "plain RANGE reply bytes unchanged by the declaration");
    assert_eq!(without, all, "and identical to the VALUES-free twin's");

    // FILTER: missing value FAILS; an uncoercible stored value is
    // excluded from a numeric range, not matched.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo"]);
    assert_eq!(r, flat_reply(&[("v:1", "10"), ("v:4", "40")]));
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"price", b"RANGE", b"0", b"6"]);
    assert_eq!(r, flat_reply(&[("v:1", "10"), ("v:2", "20"), ("v:5", "50")]));

    // FILTER pages with a cursor (driving order unchanged).
    let p1 = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo", b"LIMIT", b"1"]);
    let s1 = String::from_utf8_lossy(&p1).into_owned();
    assert!(s1.contains("v:1") && !s1.contains("v:4"), "{s1}");
    let cursor = s1.lines().nth(2).unwrap().to_string();
    assert_ne!(cursor, "0", "full filtered page carries a cursor: {s1}");
    let p2 = cmd(
        &mut c,
        &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo",
          b"LIMIT", b"5", b"CURSOR", cursor.as_bytes()],
    );
    assert_eq!(p2, flat_reply(&[("v:4", "40")]), "non-overlapping resume");

    // SORT ASC/DESC: missing value LAST in both directions; str vs i64
    // declared types order differently.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"SORT", b"city", b"ASC"]);
    assert_eq!(r, flat_reply(&[("v:5", "50"), ("v:2", "20"), ("v:6", "60"), ("v:1", "10"), ("v:4", "40"), ("v:3", "30")]));
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"SORT", b"city", b"DESC"]);
    assert_eq!(r, flat_reply(&[("v:1", "10"), ("v:4", "40"), ("v:2", "20"), ("v:6", "60"), ("v:5", "50"), ("v:3", "30")]));
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"SORT", b"price", b"ASC"]);
    assert_eq!(
        r,
        flat_reply(&[("v:2", "20"), ("v:5", "50"), ("v:1", "10"), ("v:3", "30"), ("v:4", "40"), ("v:6", "60")]),
        "numeric under TYPES i64; no-price and uncoercible-price sort last"
    );

    // DISTINCT collapses to the first per city in driving order; the
    // cityless row is its own group.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"DISTINCT", b"city"]);
    assert_eq!(r, flat_reply(&[("v:1", "10"), ("v:2", "20"), ("v:3", "30"), ("v:5", "50")]));

    // FACET: counts over the WHOLE match set before truncation, ONE
    // trailing element appended to the rows array.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FACET", b"city", b"LIMIT", b"2"]);
    let expect = "*2\r\n$1\r\n0\r\n*5\r\n$3\r\nv:1\r\n$2\r\n10\r\n$3\r\nv:2\r\n$2\r\n20\r\n\
                  *2\r\n$4\r\ncity\r\n*6\r\n$5\r\nosaka\r\n$1\r\n2\r\n$5\r\ntokyo\r\n$1\r\n2\r\n$5\r\nkyoto\r\n$1\r\n1\r\n";
    assert_eq!(String::from_utf8_lossy(&r), expect);
    // FILTER reduces the counts; DISTINCT does not.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"price", b"RANGE", b"0", b"6", b"FACET", b"city"]);
    let s = String::from_utf8_lossy(&r);
    for label in ["kyoto", "osaka", "tokyo"] {
        assert!(s.contains(&format!("{label}\r\n$1\r\n1")), "{s}");
    }
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"DISTINCT", b"city", b"FACET", b"city"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("osaka\r\n$1\r\n2") && s.contains("tokyo\r\n$1\r\n2"), "{s}");

    // OFFSET: non-overlapping pages; past the end = empty, not error.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"OFFSET", b"2", b"LIMIT", b"2"]);
    assert_eq!(r, flat_reply(&[("v:3", "30"), ("v:4", "40")]));
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"OFFSET", b"4", b"LIMIT", b"2"]);
    assert_eq!(r, flat_reply(&[("v:5", "50"), ("v:6", "60")]));
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"OFFSET", b"100"]);
    assert_eq!(r, flat_reply(&[]));

    // CURSOR × selection clauses: the named refusal.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"CURSOR", b"0", b"SORT", b"city", b"ASC"]);
    assert_eq!(
        String::from_utf8_lossy(&r),
        "-ERR IDX.QUERY 'vals': CURSOR cannot combine with SORT|DISTINCT|FACET|OFFSET\r\n"
    );

    // Clause errors name the field / the declared type.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"nope", b"EQ", b"1"]);
    assert!(String::from_utf8_lossy(&r).contains("FILTER names field 'nope'"), "{:?}", String::from_utf8_lossy(&r));
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"price", b"EQ", b"abc"]);
    assert!(String::from_utf8_lossy(&r).contains("is not a valid i64"), "{:?}", String::from_utf8_lossy(&r));

    // IDX.COUNT applies FILTER (4.2: the claused count); clauses it
    // would not apply stay refusals, not silence.
    let r = cmd(&mut c, &[b"IDX.COUNT", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo"]);
    assert_eq!(r, b":2\r\n".to_vec(), "v:1 and v:4 are tokyo");
    let r = cmd(&mut c, &[b"IDX.COUNT", b"vals", b"RANGE", b"0", b"100", b"SORT", b"city", b"ASC"]);
    assert!(r.starts_with(b"-ERR"), "{:?}", String::from_utf8_lossy(&r));

    // A live update moves the stored value with the row.
    cmd(&mut c, &[b"HSET", b"v:3", b"city", b"tokyo"]);
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo"]);
    assert_eq!(r, flat_reply(&[("v:1", "10"), ("v:3", "30"), ("v:4", "40")]));
    cmd(&mut c, &[b"DEL", b"v:1"]);
    let r = cmd(&mut c, &[b"IDX.QUERY", b"vals", b"RANGE", b"0", b"100", b"FILTER", b"city", b"EQ", b"tokyo"]);
    assert_eq!(r, flat_reply(&[("v:3", "30"), ("v:4", "40")]));
}

/// FLUSHALL must empty the index segments on THIS face too — the
/// embedded on_commit hook resets every segment on FLUSH; a server
/// that leaves them populated would serve deleted keys out of
/// IDX.QUERY (probe written during v4.1-V5; asserts the two faces
/// agree on flush semantics).
#[test]
fn flushall_empties_the_index_segments() {
    let srv = Server::start();
    let mut c = srv.connect();
    for i in 0..20u32 {
        let key = format!("u:{i}");
        assert!(
            cmd(&mut c, &[b"HSET", key.as_bytes(), b"score", format!("{i}").as_bytes()])
                .starts_with(b":")
        );
    }
    assert!(cmd(&mut c, &[
        b"IDX.CREATE", b"by_score", b"ON", b"PREFIX", b"u:", b"FIELD", b"score",
        b"TYPE", b"i64", b"KIND", b"range",
    ]).starts_with(b"+OK"));
    let full = query_ready(&mut c, &[b"IDX.QUERY", b"by_score", b"RANGE", b"0", b"100"]);
    assert!(full.starts_with(b"*"), "{}", String::from_utf8_lossy(&full));
    assert_ne!(full, b"*0\r\n".to_vec(), "populated before the flush");

    assert!(cmd(&mut c, &[b"FLUSHALL"]).starts_with(b"+OK"));
    let after = query_ready(&mut c, &[b"IDX.QUERY", b"by_score", b"RANGE", b"0", b"100"]);
    assert_eq!(
        after,
        b"*2\r\n$1\r\n0\r\n*0\r\n".to_vec(), // [cursor 0, empty page]
        "a flushed keyspace must not answer from stale index entries"
    );
}

/// The claused count: IDX.COUNT applies FILTER, totalling what a
/// claused query's pages would reach without materializing them — the
/// consumer shape it closes counted a filtered axis by fetching every
/// page and taking its length.
#[test]
fn idx_count_applies_filter() {
    let srv = Server::start();
    let mut c = srv.connect();
    for i in 0..30u32 {
        let key = format!("u:{i}");
        let dept: &[u8] = if i % 3 == 0 { b"eng" } else { b"ops" };
        assert!(cmd(&mut c, &[
            b"HSET", key.as_bytes(), b"score", format!("{i}").as_bytes(), b"dept", dept,
        ]).starts_with(b":"));
    }
    assert!(cmd(&mut c, &[
        b"IDX.CREATE", b"by_score", b"ON", b"PREFIX", b"u:", b"FIELD", b"score",
        b"TYPE", b"i64", b"KIND", b"range", b"VALUES", b"dept",
    ]).starts_with(b"+OK"));
    let _ = query_ready(&mut c, &[b"IDX.QUERY", b"by_score", b"RANGE", b"0", b"100"]);

    assert_eq!(cmd(&mut c, &[b"IDX.COUNT", b"by_score", b"RANGE", b"0", b"100"]), b":30\r\n".to_vec());
    assert_eq!(
        cmd(&mut c, &[b"IDX.COUNT", b"by_score", b"RANGE", b"0", b"100", b"FILTER", b"dept", b"EQ", b"eng"]),
        b":10\r\n".to_vec(),
        "0,3,...,27"
    );
    assert_eq!(
        cmd(&mut c, &[b"IDX.COUNT", b"by_score", b"RANGE", b"10", b"20", b"FILTER", b"dept", b"EQ", b"eng"]),
        b":3\r\n".to_vec(),
        "eng within 10..=20 is 12, 15, 18"
    );
    // A clause the count would not apply is a refusal, not silence.
    assert!(cmd(&mut c, &[b"IDX.COUNT", b"by_score", b"RANGE", b"0", b"100", b"SORT", b"dept", b"ASC"]).starts_with(b"-ERR"));
}

/// A key deleted by a MULTI-key verb must leave the index with it.
///
/// The index is maintained by `Commands::on_write`, which the dispatch
/// path calls only when the resolver produced a single `key_idx`.
/// Multi-key `DEL`/`UNLINK`, and the cross-shard `RENAME` two-step,
/// route by key without one and used to execute their op on the owning
/// shard without ever telling the index: `IDX.QUERY` kept answering
/// with rows that no longer existed (hydration nil, sort value intact),
/// `IDX.COUNT` kept counting them, and nothing repaired it — only
/// `IDX.VERIFY` could see the drift. That breaks the
/// derived-by-construction invariant the whole IDX surface rests on.
#[test]
fn multi_key_delete_and_rename_keep_the_index_honest() {
    let srv = Server::start();
    let mut c = srv.connect();

    for i in 0..24 {
        cmd(
            &mut c,
            &[b"HSET", format!("row:{i}").as_bytes(), b"age", format!("{}", 20 + i).as_bytes()],
        );
    }
    assert_eq!(
        cmd(
            &mut c,
            &[b"IDX.CREATE", b"byage", b"ON", b"PREFIX", b"row:", b"FIELD", b"age", b"TYPE", b"i64", b"KIND", b"range"],
        ),
        b"+OK\r\n"
    );
    let indexed = |c: &mut std::net::TcpStream| -> String {
        String::from_utf8_lossy(&query_ready(
            c,
            &[b"IDX.QUERY", b"byage", b"RANGE", b"0", b"999", b"LIMIT", b"200"],
        ))
        .into_owned()
    };
    let before = indexed(&mut c);
    for i in 0..24 {
        assert!(before.contains(&format!("row:{i}\r\n")), "row:{i} must be indexed first");
    }

    // Two keys per verb, so each call is genuinely multi-key and spans
    // shards (24 rows over 8 shards).
    cmd(&mut c, &[b"DEL", b"row:7", b"row:11"]);
    cmd(&mut c, &[b"UNLINK", b"row:5", b"row:6"]);
    cmd(&mut c, &[b"RENAME", b"row:12", b"moved:12"]);
    std::thread::sleep(std::time::Duration::from_millis(300));

    let after = indexed(&mut c);
    for gone in ["row:7", "row:11", "row:5", "row:6", "row:12"] {
        assert!(!after.contains(&format!("{gone}\r\n")), "{gone} still answers IDX.QUERY");
    }
    for kept in ["row:8", "row:23", "row:0"] {
        assert!(after.contains(&format!("{kept}\r\n")), "{kept} must survive");
    }

    // The engine's own auditor agrees, and a later write still indexes.
    let v = cmd(&mut c, &[b"IDX.VERIFY", b"byage"]);
    let v = String::from_utf8_lossy(&v);
    assert!(v.contains("drift"), "VERIFY shape changed: {v}");
    let drift = v.split("drift\r\n$").nth(1).and_then(|s| s.split("\r\n").nth(1));
    assert_eq!(drift, Some("0"), "VERIFY still reports drift: {v}");
    cmd(&mut c, &[b"HSET", b"row:99", b"age", b"77"]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(indexed(&mut c).contains("row:99\r\n"), "a fresh write must still index");
}

/// A row that arrives by scope migration must be indexed like any other.
///
/// `MOVE-SCOPE-INGEST` replays the emitted frames straight into the
/// store (`ops::scope_move`), which is not the write path, so the
/// derived structures never heard about the rows: they existed in the
/// keyspace and were invisible to every `IDX.QUERY`. Worse than the
/// stale-entry direction, `IDX.VERIFY` cannot see this one either — it
/// audits index entries against the store, not store rows against the
/// index — so a migrated-into node under-answered silently.
#[test]
fn scope_ingested_rows_enter_the_index() {
    let srv = Server::start();
    let mut c = srv.connect();

    for i in 0..8 {
        cmd(&mut c, &[b"HSET", format!("row:{i}").as_bytes(), b"age", format!("{}", 20 + i).as_bytes()]);
    }
    assert_eq!(
        cmd(
            &mut c,
            &[b"IDX.CREATE", b"byage", b"ON", b"PREFIX", b"row:", b"FIELD", b"age", b"TYPE", b"i64", b"KIND", b"range"],
        ),
        b"+OK\r\n"
    );
    let indexed = |c: &mut std::net::TcpStream| -> String {
        String::from_utf8_lossy(&query_ready(
            c,
            &[b"IDX.QUERY", b"byage", b"RANGE", b"0", b"999", b"LIMIT", b"200"],
        ))
        .into_owned()
    };
    assert!(indexed(&mut c).contains("row:0\r\n"));

    // Exactly what a migration source ships: `<VERB> <key> …` frames.
    let bulk: &[u8] = b"*4\r\n$4\r\nHSET\r\n$6\r\nrow:50\r\n$3\r\nage\r\n$2\r\n60\r\n\
*4\r\n$4\r\nHSET\r\n$6\r\nrow:51\r\n$3\r\nage\r\n$2\r\n61\r\n";
    let r = cmd(&mut c, &[b"MOVE-SCOPE-INGEST", b"row:", bulk]);
    assert!(r.starts_with(b"+OK 2"), "ingest failed: {}", String::from_utf8_lossy(&r));
    std::thread::sleep(std::time::Duration::from_millis(300));

    let after = indexed(&mut c);
    for row in ["row:50", "row:51"] {
        assert!(after.contains(&format!("{row}\r\n")), "{row} arrived but never indexed");
    }
    assert_eq!(cmd(&mut c, &[b"IDX.COUNT", b"byage", b"RANGE", b"0", b"999"]), b":10\r\n");
}
