//! T7 e2e — the TABLE.* declaration layer against a real in-process
//! 8-shard server (tier_hydration harness genre): the R1-R12
//! conformance suite (RFC 2026-07-24-virtual-rds-views-arc §1) plus
//! C2 (VERIFY clean) and C6 (index-only proof on a tiered store).
//!
//! Law 3 throughout: the table is a declaration compiled into
//! explicitly-named IDX access paths; nothing here parses SQL, plans a
//! query, or enforces a schema at query time.

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
    /// `tier_budget = None` → an untiered server.
    fn start(tier_budget: Option<u64>) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-tablee2e-{}",
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
                .with_data_dir(dir_thread)
                .with_tier_budget(tier_budget);
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

// ---- reply helpers -----------------------------------------------------

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

/// The keys of a scalar `[cursor, [k, v, k, v…]]` page, wire order.
fn page_keys(reply: &[u8]) -> Vec<Vec<u8>> {
    let b = bulks(reply);
    // b[0] is the cursor; then k, v alternating.
    b[1..].chunks(2).map(|p| p[0].clone()).collect()
}

/// The value paired with `label` in a label/value bulk sequence.
fn labeled(fields: &[Vec<u8>], label: &[u8]) -> Vec<Vec<u8>> {
    fields
        .windows(2)
        .filter(|w| w[0] == label)
        .map(|w| w[1].clone())
        .collect()
}

fn keys_eq(reply: &[u8], want: &[&str]) {
    let got: Vec<String> =
        page_keys(reply).iter().map(|k| String::from_utf8_lossy(k).into_owned()).collect();
    assert_eq!(got, want.to_vec(), "reply: {}", String::from_utf8_lossy(reply));
}

/// Pull one integer gauge out of an INFO reply.
fn info_gauge(s: &mut std::net::TcpStream, name: &str) -> u64 {
    let reply = String::from_utf8_lossy(&cmd(s, &[b"INFO"])).into_owned();
    reply
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}:")))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("INFO has no {name} gauge:\n{reply}"))
}

fn wait_cold_stable(s: &mut std::net::TcpStream) -> u64 {
    let mut last = 0u64;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let now = info_gauge(s, "cold_keys");
        if now > 0 && now == last {
            return now;
        }
        last = now;
    }
    panic!("cold_keys never stabilized (last = {last})");
}

// ---- the shared "user" table -------------------------------------------

const DECLARE_USER: &[&[u8]] = &[
    b"TABLE.DECLARE", b"user", b"PREFIX", b"u:", b"PK", b"id",
    b"COLUMN", b"id", b"str", b"COLUMN", b"name", b"str", b"COLUMN", b"age", b"i64",
    b"COLUMN", b"dept", b"str", b"COLUMN", b"email", b"str", b"COLUMN", b"deleted", b"i64",
    b"INDEX", b"age", b"RANGE", b"VALUES", b"dept", b"name", b"deleted",
    b"INDEX", b"dept", b"RANGE",
    b"INDEX", b"email", b"UNIQUE",
    b"ORDERPATH", b"by_dept_age", b"ON", b"dept", b"THEN", b"age", b"DESC",
];

/// (key, name, age, dept, email) — u:6 deliberately has NO dept (the
/// composite's missing-column exclusion) but is a full row otherwise.
const ROWS: &[(&str, &str, &str, &str, &str)] = &[
    ("u:1", "alice", "30", "eng", "a@x"),
    ("u:2", "bob", "45", "eng", "b@x"),
    ("u:3", "carol", "25", "ops", "c@x"),
    ("u:4", "dave", "38", "eng", "d@x"),
    ("u:5", "erin", "52", "ops", "e@x"),
    ("u:6", "frank", "61", "", "f@x"),
];

fn write_rows(s: &mut std::net::TcpStream) {
    for (key, name, age, dept, email) in ROWS {
        let mut argv: Vec<&[u8]> = vec![
            b"HSET", key.as_bytes(), b"id", &key.as_bytes()[2..], b"name", name.as_bytes(),
            b"age", age.as_bytes(), b"email", email.as_bytes(), b"deleted", b"0",
        ];
        if !dept.is_empty() {
            argv.push(b"dept");
            argv.push(dept.as_bytes());
        }
        let reply = cmd(s, &argv);
        assert!(reply.starts_with(b":"), "HSET {key}: {}", String::from_utf8_lossy(&reply));
    }
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

/// Declare + write + wait until every compiled path serves.
fn setup_user(c: &mut std::net::TcpStream) {
    assert_eq!(cmd(c, DECLARE_USER), b"+OK\r\n", "TABLE.DECLARE");
    write_rows(c);
    wait_ready(c, &[b"IDX.QUERY", b"user.age", b"EQ", b"0"]);
    wait_ready(c, &[b"IDX.QUERY", b"user.dept", b"EQ", b"x"]);
    wait_ready(c, &[b"IDX.QUERY", b"user.email", b"EQ", b"x"]);
    wait_ready(c, &[b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"x"]);
}

// =========================================================================
// R1-R8, R10-R11, C2, C3 against one hot server; C6 on a tiered one.
// =========================================================================

#[test]
fn r1_declare_round_trip_and_compiled_paths() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    setup_user(&mut c);
    // The declaration round-trips through TABLE.LIST…
    let list = bulks(&cmd(&mut c, &[b"TABLE.LIST"]));
    assert_eq!(labeled(&list, b"name"), vec![b"user".to_vec()]);
    assert_eq!(labeled(&list, b"prefix"), vec![b"u:".to_vec()]);
    assert_eq!(labeled(&list, b"pk"), vec![b"id".to_vec()]);
    assert_eq!(labeled(&list, b"columns"), vec![b"6".to_vec()]);
    assert_eq!(labeled(&list, b"indexes"), vec![b"3".to_vec()]);
    assert_eq!(labeled(&list, b"orderpaths"), vec![b"1".to_vec()]);
    // …and its compiled access paths are ordinary, named indexes.
    let idx = bulks(&cmd(&mut c, &[b"IDX.LIST"]));
    let names = labeled(&idx, b"name");
    for want in ["user.age", "user.dept", "user.email", "user.by_dept_age"] {
        assert!(names.iter().any(|n| n == want.as_bytes()), "IDX.LIST missing {want}");
    }
    // DROP cascades to the compiled indexes.
    assert_eq!(cmd(&mut c, &[b"TABLE.DROP", b"user"]), b":1\r\n");
    let idx = bulks(&cmd(&mut c, &[b"IDX.LIST"]));
    assert!(labeled(&idx, b"name").is_empty(), "compiled indexes must drop with the table");
}

#[test]
fn r2_r3_r4_point_range_compose_and_residual_filter() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    setup_user(&mut c);
    // R2 — point + range through the compiled paths.
    keys_eq(&cmd(&mut c, &[b"IDX.QUERY", b"user.age", b"RANGE", b"25", b"45"]),
        &["u:3", "u:1", "u:4", "u:2"]);
    keys_eq(&cmd(&mut c, &[b"IDX.QUERY", b"user.dept", b"EQ", b"ops"]), &["u:3", "u:5"]);
    keys_eq(&cmd(&mut c, &[b"IDX.QUERY", b"user.email", b"EQ", b"d@x"]), &["u:4"]);
    // R3 — AND/OR composition over two compiled indexes.
    let and = cmd(&mut c, &[
        b"IDX.QUERY", b"COMPOSE", b"AND", b"user.age", b"RANGE", b"30", b"50",
        b"user.dept", b"EQ", b"eng",
    ]);
    let and_keys: Vec<Vec<u8>> = bulks(&and)[1..].to_vec();
    assert_eq!(and_keys, vec![b"u:1".to_vec(), b"u:2".to_vec(), b"u:4".to_vec()], "AND, key order");
    let or = cmd(&mut c, &[
        b"IDX.QUERY", b"COMPOSE", b"OR", b"user.age", b"RANGE", b"60", b"100",
        b"user.dept", b"EQ", b"ops",
    ]);
    let or_keys: Vec<Vec<u8>> = bulks(&or)[1..].to_vec();
    assert_eq!(or_keys, vec![b"u:3".to_vec(), b"u:5".to_vec(), b"u:6".to_vec()]);
    // R4 — residual FILTER on a declared VALUES column (T2's G1).
    keys_eq(&cmd(&mut c, &[
        b"IDX.QUERY", b"user.age", b"RANGE", b"0", b"100", b"FILTER", b"dept", b"EQ", b"eng",
    ]), &["u:1", "u:4", "u:2"]);
}

#[test]
fn r5_order_by_single_and_composite_orderpath() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    setup_user(&mut c);
    // Single-column ORDER BY = the SORT clause over a stored VALUES column.
    keys_eq(&cmd(&mut c, &[
        b"IDX.QUERY", b"user.age", b"RANGE", b"0", b"100", b"SORT", b"name", b"ASC",
    ]), &["u:1", "u:2", "u:3", "u:4", "u:5", "u:6"]);
    // Composite ORDERPATH: equality prefix, DESC component.
    keys_eq(&cmd(&mut c, &[
        b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"eng",
    ]), &["u:2", "u:4", "u:1"]);
    // Equality prefix + range on the next component (still age DESC).
    keys_eq(&cmd(&mut c, &[
        b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"eng",
        b"RANGE", b"age", b"31", b"46",
    ]), &["u:2", "u:4"]);
    // Missing-column exclusion: u:6 (no dept) exists on the plain path…
    keys_eq(&cmd(&mut c, &[b"IDX.QUERY", b"user.age", b"EQ", b"61"]), &["u:6"]);
    // …but is EXCLUDED from the composite: eng(3) + ops(2) = 5 of 6 rows.
    assert_eq!(
        cmd(&mut c, &[b"IDX.COUNT", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"eng"]),
        b":3\r\n"
    );
    assert_eq!(
        cmd(&mut c, &[b"IDX.COUNT", b"user.by_dept_age", b"WHERE", b"dept", b"EQ", b"ops"]),
        b":2\r\n"
    );
}

#[test]
fn r6_limit_offset_two_pages_do_not_overlap() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    setup_user(&mut c);
    let p1 = page_keys(&cmd(&mut c, &[
        b"IDX.QUERY", b"user.age", b"RANGE", b"0", b"100", b"LIMIT", b"3",
    ]));
    let p2 = page_keys(&cmd(&mut c, &[
        b"IDX.QUERY", b"user.age", b"RANGE", b"0", b"100", b"LIMIT", b"3", b"OFFSET", b"3",
    ]));
    assert_eq!(p1.len(), 3);
    assert_eq!(p2.len(), 3);
    assert!(p1.iter().all(|k| !p2.contains(k)), "pages must not overlap: {p1:?} vs {p2:?}");
    let mut union: Vec<Vec<u8>> = p1.into_iter().chain(p2).collect();
    union.sort();
    assert_eq!(union.len(), 6, "the two pages must cover the whole range");
}

#[test]
fn r7_count_and_the_agg_kind_is_a_named_refusal() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    setup_user(&mut c);
    assert_eq!(cmd(&mut c, &[b"IDX.COUNT", b"user.age", b"RANGE", b"0", b"100"]), b":6\r\n");
    assert_eq!(cmd(&mut c, &[b"IDX.COUNT", b"user.dept", b"EQ", b"eng"]), b":3\r\n");
    // Agg indexes are NOT table-compiled in v1: the grammar refuses the
    // kind by name (declare one directly via IDX.CREATE KIND agg).
    assert_eq!(
        cmd(&mut c, &[
            b"TABLE.DECLARE", b"t2", b"PREFIX", b"t2:", b"PK", b"id",
            b"COLUMN", b"id", b"str", b"COLUMN", b"n", b"i64", b"INDEX", b"n", b"agg",
        ]),
        b"-ERR INDEX kind must be range|unique\r\n"
    );
}

#[test]
fn r8_via_lookup_over_a_compiled_index_unchanged() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    setup_user(&mut c);
    assert_eq!(
        cmd(&mut c, &[
            b"VIEW.CREATE", b"engers", b"QUERY", b"user.dept", b"EQ", b"eng",
            b"ORDER", b"BY", b"user.age", b"VIA", b"{key}",
        ]),
        b"+OK\r\n"
    );
    let reply = cmd(&mut c, &[b"VIEW.QUERY", b"engers", b"FIELDS", b"name"]);
    let b = bulks(&reply);
    for name in [b"alice".as_slice(), b"bob", b"dave"] {
        assert!(b.iter().any(|v| v == name), "VIA hydration missing {}: {}",
            String::from_utf8_lossy(name), String::from_utf8_lossy(&reply));
    }
}

#[test]
fn r10_optimistic_lock_via_watch_multi_exec() {
    let srv = Server::start(None);
    let mut c1 = srv.connect();
    let mut c2 = srv.connect();
    setup_user(&mut c1);
    // Clean CAS: WATCH → read → MULTI → write → EXEC applies.
    assert_eq!(cmd(&mut c1, &[b"WATCH", b"u:1"]), b"+OK\r\n");
    assert!(cmd(&mut c1, &[b"HGET", b"u:1", b"age"]).starts_with(b"$2\r\n30"));
    assert_eq!(cmd(&mut c1, &[b"MULTI"]), b"+OK\r\n");
    assert_eq!(cmd(&mut c1, &[b"HSET", b"u:1", b"age", b"31"]), b"+QUEUED\r\n");
    let exec = cmd(&mut c1, &[b"EXEC"]);
    assert!(exec.starts_with(b"*1\r\n"), "clean EXEC applies: {}", String::from_utf8_lossy(&exec));
    // Conflicting CAS: a second writer touches the watched row → EXEC aborts.
    assert_eq!(cmd(&mut c1, &[b"WATCH", b"u:2"]), b"+OK\r\n");
    assert!(cmd(&mut c2, &[b"HSET", b"u:2", b"age", b"46"]).starts_with(b":"));
    assert_eq!(cmd(&mut c1, &[b"MULTI"]), b"+OK\r\n");
    assert_eq!(cmd(&mut c1, &[b"HSET", b"u:2", b"age", b"99"]), b"+QUEUED\r\n");
    assert_eq!(cmd(&mut c1, &[b"EXEC"]), b"*-1\r\n", "conflicted EXEC must abort");
    // The row carries the OTHER writer's value; the index followed it.
    wait_ready(&mut c1, &[b"IDX.QUERY", b"user.age", b"EQ", b"46"]);
    keys_eq(&cmd(&mut c1, &[b"IDX.QUERY", b"user.age", b"EQ", b"46"]), &["u:2"]);
}

#[test]
fn r11_uniqueness_sequence_and_soft_delete_recipes() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    setup_user(&mut c);
    // Uniqueness is verified, not enforced: a duplicate email WRITES,
    // the read path exposes every holder, and the per-shard duplicate
    // counter reports it. The `duplicates` stat is per shard, so nine
    // holders across eight shards guarantee (pigeonhole) at least one
    // same-shard collision — deterministic, not placement luck.
    assert!(cmd(&mut c, &[b"HSET", b"u:2", b"email", b"a@x"]).starts_with(b":"));
    keys_eq(&cmd(&mut c, &[b"IDX.QUERY", b"user.email", b"EQ", b"a@x"]), &["u:1", "u:2"]);
    for i in 7..14u32 {
        let key = format!("u:{i}");
        let reply = cmd(&mut c, &[b"HSET", key.as_bytes(), b"id", format!("{i}").as_bytes(), b"email", b"a@x"]);
        assert!(reply.starts_with(b":"), "HSET {key}");
    }
    let verify = bulks(&cmd(&mut c, &[b"IDX.VERIFY", b"user.email"]));
    let dup: u64 = String::from_utf8_lossy(&labeled(&verify, b"duplicates")[0]).parse().unwrap();
    assert!(dup >= 1, "the fence must report a same-shard duplicate, got {dup}");
    // Sequence recipe: an INCR block allocates ids.
    assert_eq!(cmd(&mut c, &[b"INCR", b"user:seq"]), b":1\r\n");
    assert_eq!(cmd(&mut c, &[b"INCR", b"user:seq"]), b":2\r\n");
    // Soft delete: a flag column + residual FILTER (deleted stays a
    // declared VALUES column of user.age).
    assert!(cmd(&mut c, &[b"HSET", b"u:5", b"deleted", b"1"]).starts_with(b":"));
    keys_eq(&cmd(&mut c, &[
        b"IDX.QUERY", b"user.age", b"RANGE", b"0", b"100", b"FILTER", b"deleted", b"EQ", b"0",
    ]), &["u:3", "u:1", "u:4", "u:2", "u:6"]);
}

#[test]
fn c2_table_verify_clean_after_declare_and_writes() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    setup_user(&mut c);
    let verify = bulks(&cmd(&mut c, &[b"TABLE.VERIFY", b"user"]));
    let names = labeled(&verify, b"index");
    assert_eq!(names.len(), 4, "one element per compiled index");
    for d in labeled(&verify, b"drift") {
        assert_eq!(d, b"0".to_vec(), "a hook-maintained compiled index must not drift");
    }
    assert_eq!(labeled(&verify, b"spotcheck_rows"), vec![b"6".to_vec()]);
    assert_eq!(labeled(&verify, b"spotcheck_type_mismatches"), vec![b"0".to_vec()]);
    // entries: age 6, dept 5 (u:6 has none), email 6, composite 5.
    assert_eq!(
        labeled(&verify, b"entries"),
        vec![b"6".to_vec(), b"5".to_vec(), b"6".to_vec(), b"5".to_vec()]
    );
    // Poison one typed column and the spot check names it.
    assert!(cmd(&mut c, &[b"HSET", b"u:9", b"id", b"9", b"age", b"not-a-number"]).starts_with(b":"));
    let verify = bulks(&cmd(&mut c, &[b"TABLE.VERIFY", b"user"]));
    assert_eq!(labeled(&verify, b"spotcheck_type_mismatches"), vec![b"1".to_vec()]);
    // v4.1-V4: the row→index direction classifies the poisoned row by
    // cause, fresh at every call — absence is NULL, never a failure.
    assert_eq!(labeled(&verify, b"rows"), vec![b"7".to_vec(); 4], "every prefix row walked");
    assert_eq!(
        labeled(&verify, b"coerce_failures"),
        vec![b"1".to_vec(), b"0".to_vec(), b"0".to_vec(), b"0".to_vec()],
        "u:9's age is present-but-not-i64 — on the age index only"
    );
    assert_eq!(
        labeled(&verify, b"absent"),
        vec![b"0".to_vec(), b"2".to_vec(), b"1".to_vec(), b"2".to_vec()],
        "u:6 and u:9 lack dept, u:9 lacks email — counted as NULL, not coercion"
    );
    assert_eq!(labeled(&verify, b"missing"), vec![b"0".to_vec(); 4], "no forgotten writer");
}

#[test]
fn r9_c3_the_refusal_surface_errors_by_name() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    setup_user(&mut c);
    // Ad-hoc SQL is not a command surface: the verbs simply do not
    // exist (or, for SELECT — a Redis verb — the arity errors).
    assert!(cmd(&mut c, &[b"INSERT", b"INTO", b"user"]).starts_with(b"-ERR unknown command"));
    assert!(cmd(&mut c, &[b"JOIN", b"user", b"orders"]).starts_with(b"-ERR unknown command"));
    assert!(cmd(&mut c, &[b"HAVING", b"count"]).starts_with(b"-ERR unknown command"));
    assert!(cmd(&mut c, &[b"SELECT", b"*", b"FROM", b"user"]).starts_with(b"-ERR"));
    // TABLE.DECLARE refusals are named, never silent.
    assert_eq!(
        cmd(&mut c, &[b"TABLE.DECLARE", b"t3", b"PREFIX", b"t3:", b"PK", b"id",
            b"COLUMN", b"id", b"str", b"INDEX", b"ghost", b"RANGE"]),
        b"-ERR INDEX names unknown column 'ghost'\r\n"
    );
    assert_eq!(
        cmd(&mut c, &[b"TABLE.DECLARE", b"t3", b"PREFIX", b"t3:", b"PK", b"id",
            b"COLUMN", b"id", b"uuid"]),
        b"-ERR COLUMN type must be i64|f64|str\r\n"
    );
    // WHERE on a non-composite index is a named error, never a scan.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"user.age", b"WHERE", b"dept", b"EQ", b"eng"]);
    assert!(
        r.starts_with(b"-ERR IDX.QUERY 'user.age': WHERE requires a composite index"),
        "{}",
        String::from_utf8_lossy(&r)
    );
    // WHERE columns must be a leading prefix of the declared order.
    let r = cmd(&mut c, &[b"IDX.QUERY", b"user.by_dept_age", b"WHERE", b"age", b"EQ", b"30"]);
    assert!(
        r.starts_with(b"-ERR IDX.QUERY 'user.by_dept_age': WHERE columns must be a leading prefix"),
        "{}",
        String::from_utf8_lossy(&r)
    );
}

/// C6 — the index-only proof on a TIERED store (the D2 shape): a
/// compiled index with VALUES answers FILTER/SORT/COUNT with ZERO row
/// touches on fully-cold rows; FIELDS hydration pays exactly one read
/// per returned row.
#[test]
fn c6_index_only_queries_touch_zero_cold_rows() {
    let srv = Server::start(Some(256 * 1024));
    let mut c = srv.connect();
    // ~8 KB pad per row forces the spill; only n/c serve the queries.
    let pad = vec![b'p'; 8192];
    for i in 0..60u32 {
        let key = format!("row:{i:02}");
        let n = i.to_string();
        let cval = if i % 2 == 0 { "even" } else { "odd" };
        let reply = cmd(&mut c, &[
            b"HSET", key.as_bytes(), b"id", n.as_bytes(), b"n", n.as_bytes(),
            b"c", cval.as_bytes(), b"pad", &pad,
        ]);
        assert!(reply.starts_with(b":"), "HSET {key}");
    }
    let cold_keys = wait_cold_stable(&mut c);
    assert!(cold_keys > 8, "need most rows cold, got {cold_keys}");
    // Declare AFTER the rows went cold — the backfill runs on the peek.
    assert_eq!(
        cmd(&mut c, &[
            b"TABLE.DECLARE", b"bench", b"PREFIX", b"row:", b"PK", b"id",
            b"COLUMN", b"id", b"str", b"COLUMN", b"n", b"i64", b"COLUMN", b"c", b"str",
            b"INDEX", b"n", b"RANGE", b"VALUES", b"c",
        ]),
        b"+OK\r\n"
    );
    wait_ready(&mut c, &[b"IDX.QUERY", b"bench.n", b"RANGE", b"0", b"0"]);
    std::thread::sleep(std::time::Duration::from_millis(300)); // gauges tick
    assert_eq!(info_gauge(&mut c, "promotions_total"), 0, "backfill must not promote");

    // Index-only: FILTER / SORT / COUNT without FIELDS = zero preads.
    let pre = info_gauge(&mut c, "peek_preads_total");
    let filtered = page_keys(&cmd(&mut c, &[
        b"IDX.QUERY", b"bench.n", b"RANGE", b"0", b"100", b"FILTER", b"c", b"EQ", b"even",
        b"LIMIT", b"100",
    ]));
    assert_eq!(filtered.len(), 30);
    let sorted = page_keys(&cmd(&mut c, &[
        b"IDX.QUERY", b"bench.n", b"RANGE", b"0", b"100", b"SORT", b"c", b"ASC", b"LIMIT", b"10",
    ]));
    assert_eq!(sorted.len(), 10);
    assert_eq!(cmd(&mut c, &[b"IDX.COUNT", b"bench.n", b"RANGE", b"0", b"100"]), b":60\r\n");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        info_gauge(&mut c, "peek_preads_total") - pre,
        0,
        "index-only queries must touch ZERO rows (the D2 shape)"
    );
    assert_eq!(info_gauge(&mut c, "promotions_total"), 0);

    // With FIELDS: exactly one read per RETURNED row, still no promotion.
    let pre = info_gauge(&mut c, "peek_preads_total");
    let page = cmd(&mut c, &[
        b"IDX.QUERY", b"bench.n", b"RANGE", b"0", b"9", b"LIMIT", b"10", b"FIELDS", b"c", b"n",
    ]);
    let returned = bulks(&page).iter().filter(|b| b.starts_with(b"row:")).count() as u64;
    assert_eq!(returned, 10);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let preads = info_gauge(&mut c, "peek_preads_total") - pre;
    // Cold rows pay one read each; rows still hot (under the watermark)
    // pay none — so preads ≤ returned, and 2 FIELDS ≠ 2 reads.
    assert!(preads <= returned, "one read per ROW at most: {preads} for {returned} rows");
    assert!(preads > 0, "a mostly-cold page must have paid some reads");
    assert_eq!(info_gauge(&mut c, "promotions_total"), 0, "hydration is not an access signal");
}

/// D2 proper (tablegate L7): a FULLY-cold table — budget crushed to 1 byte
/// saturates the demote target to 0, so the tick drains EVERY spillable row
/// — then index-only queries must pay zero cold reads. This is the fixture
/// the mostly-cold c6 test cannot provide; the T9 envelope re-runs the same
/// shape at 10M-row scale on the bench box.
#[test]
fn d2_fully_cold_table_index_only_queries_pay_zero_cold_reads() {
    let srv = Server::start(Some(256 * 1024));
    let mut c = srv.connect();
    let pad = vec![b'p'; 8192];
    for i in 0..60u32 {
        let key = format!("row:{i:02}");
        let n = i.to_string();
        let cval = if i % 2 == 0 { "even" } else { "odd" };
        let reply = cmd(&mut c, &[
            b"HSET", key.as_bytes(), b"id", n.as_bytes(), b"n", n.as_bytes(),
            b"c", cval.as_bytes(), b"pad", &pad,
        ]);
        assert!(reply.starts_with(b":"), "HSET {key}");
    }
    assert_eq!(
        cmd(&mut c, &[
            b"TABLE.DECLARE", b"bench", b"PREFIX", b"row:", b"PK", b"id",
            b"COLUMN", b"id", b"str", b"COLUMN", b"n", b"i64", b"COLUMN", b"c", b"str",
            b"INDEX", b"n", b"RANGE", b"VALUES", b"c",
        ]),
        b"+OK\r\n"
    );
    wait_ready(&mut c, &[b"IDX.QUERY", b"bench.n", b"RANGE", b"0", b"0"]);
    // Crush the budget: the demote target saturates to 0 and the shard
    // tick drains EVERY spillable value — the fully-cold state.
    assert_eq!(cmd(&mut c, &[b"CONFIG", b"SET", b"tiering-budget", b"1"]), b"+OK\r\n");
    let mut cold = 0;
    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        cold = info_gauge(&mut c, "cold_keys");
        if cold >= 60 {
            break;
        }
    }
    assert!(cold >= 60, "every row must be cold, got {cold}");

    let pre_preads = info_gauge(&mut c, "peek_preads_total");
    let pre_promo = info_gauge(&mut c, "promotions_total");
    let filtered = page_keys(&cmd(&mut c, &[
        b"IDX.QUERY", b"bench.n", b"RANGE", b"0", b"100", b"FILTER", b"c", b"EQ", b"even",
        b"LIMIT", b"100",
    ]));
    assert_eq!(filtered.len(), 30);
    let sorted = page_keys(&cmd(&mut c, &[
        b"IDX.QUERY", b"bench.n", b"RANGE", b"0", b"100", b"SORT", b"c", b"DESC", b"LIMIT", b"10",
    ]));
    assert_eq!(sorted.len(), 10);
    assert_eq!(cmd(&mut c, &[b"IDX.COUNT", b"bench.n", b"RANGE", b"0", b"100"]), b":60\r\n");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        info_gauge(&mut c, "peek_preads_total") - pre_preads,
        0,
        "index-only queries on a FULLY-cold table must touch zero rows"
    );
    assert_eq!(info_gauge(&mut c, "promotions_total") - pre_promo, 0);

    // And with FIELDS on the fully-cold table: EXACTLY one read per row.
    let pre = info_gauge(&mut c, "peek_preads_total");
    let page = cmd(&mut c, &[
        b"IDX.QUERY", b"bench.n", b"RANGE", b"0", b"9", b"LIMIT", b"10", b"FIELDS", b"c", b"n",
    ]);
    let returned = bulks(&page).iter().filter(|b| b.starts_with(b"row:")).count() as u64;
    assert_eq!(returned, 10);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        info_gauge(&mut c, "peek_preads_total") - pre,
        returned,
        "fully-cold: exactly one read per returned row"
    );
    assert_eq!(info_gauge(&mut c, "promotions_total") - pre_promo, 0);
}

/// R12+ — the boot verbs (v4.1-V3, dogfood F8.2): ENSURE answers
/// +OK on first declare, +UNCHANGED on the identical re-declare (the
/// steady state of a declare-at-boot caller), and refuses a changed
/// spec by naming which part differs. REPLACE is the explicit rebuild,
/// and a bad replacement refuses before the old table drops.
#[test]
fn ensure_and_replace_are_the_boot_verbs() {
    let srv = Server::start(None);
    let mut c = srv.connect();
    let ensure_user: Vec<&[u8]> = {
        let mut v = DECLARE_USER.to_vec();
        v[0] = b"TABLE.ENSURE";
        v
    };
    assert_eq!(cmd(&mut c, &ensure_user), b"+OK\r\n", "first boot declares");
    assert_eq!(cmd(&mut c, &ensure_user), b"+UNCHANGED\r\n", "every later boot is a no-op");

    // A changed spec refuses and names the differing part.
    let mut changed = DECLARE_USER.to_vec();
    changed[0] = b"TABLE.ENSURE";
    changed.extend_from_slice(&[b"COLUMN", b"extra", b"i64"]);
    let r = cmd(&mut c, &changed);
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("-ERR"), "changed spec must refuse: {s}");
    assert!(s.contains("COLUMNS"), "the refusal names what changed: {s}");

    // A bad REPLACE refuses before the old table drops.
    let bad: &[&[u8]] = &[
        b"TABLE.REPLACE", b"user", b"PREFIX", b"u:", b"PK", b"id",
        b"COLUMN", b"id", b"str",
        b"ORDERPATH", b"by_ghost", b"ON", b"ghost",
    ];
    let r = cmd(&mut c, bad);
    assert!(r.starts_with(b"-ERR"), "bad replacement refuses");
    let list = cmd(&mut c, &[b"TABLE.LIST"]);
    assert!(
        String::from_utf8_lossy(&list).contains("user"),
        "the old table must still stand after a refused REPLACE"
    );

    // A good REPLACE installs the new shape.
    let mut good = DECLARE_USER.to_vec();
    good[0] = b"TABLE.REPLACE";
    good.extend_from_slice(&[b"COLUMN", b"extra", b"i64"]);
    assert_eq!(cmd(&mut c, &good), b"+OK\r\n", "explicit rebuild");
}
