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
