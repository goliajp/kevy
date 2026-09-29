//! A global index against a real 4-shard reactor: rows stay on the shard
//! their key hashes to, their entries go to the partition their value falls
//! in. It must answer what a local index over the same rows answers, and a
//! write must be visible to a query sent the moment its reply arrives.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::common::Wire;

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(shards: usize) -> Self {
        let port = kevy_testnet::free_port();
        let dir = std::env::temp_dir().join(format!("kevy-gidx-{}-{port}", std::process::id()));
        Self::start_at(dir, shards)
    }

    /// A server over `dir`, which it removes when dropped.
    fn start_at(dir: std::path::PathBuf, shards: usize) -> Self {
        let port = kevy_testnet::free_port();
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop_thread, dir_thread) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(shards))
                .bind([127, 0, 0, 1], port)
                .shards(shards)
                .with_data_dir(dir_thread)
                .run(stop_thread)
                .unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn wire(&self) -> Wire {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        Wire::new(s)
    }
}

impl Server {
    fn halt(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.halt();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn call(w: &mut Wire, q: &[&[u8]]) -> Vec<u8> {
    w.call(q)
}

fn ready(w: &mut Wire, q: &[&[u8]]) -> Vec<u8> {
    for _ in 0..200 {
        let r = call(w, q);
        if !r.starts_with(b"-INDEXBUILDING") {
            return r;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the index never became ready");
}

fn text(r: &[u8]) -> String {
    String::from_utf8_lossy(r).into_owned()
}

fn create(w: &mut Wire, name: &[u8], tail: &[&[u8]]) {
    let mut q: Vec<&[u8]> = vec![
        b"IDX.CREATE",
        name,
        b"ON",
        b"PREFIX",
        b"user:",
        b"FIELD",
        b"age",
        b"TYPE",
        b"i64",
        b"KIND",
        b"range",
    ];
    q.extend_from_slice(tail);
    assert_eq!(call(w, &q), b"+OK\r\n", "create {}", text(name));
}

#[test]
fn a_local_backfill_beside_a_global_index_misses_no_row() {
    for global_first in [false, true] {
        let srv = Server::start(4);
        let mut w = srv.wire();
        for i in 0..8000u32 {
            let (key, age) = (format!("user:{i}"), ((i * 7919) % 1000).to_string());
            call(&mut w, &[b"HSET", key.as_bytes(), b"age", age.as_bytes()]);
        }
        if global_first {
            create(&mut w, b"age_g", &[b"PARTITION", b"global"]);
            let _ = described_splits(&mut w, b"age_g");
        }
        create(&mut w, b"age_l", &[]);
        if global_first {
            wait_ready(&mut w, b"age_g");
        }
        wait_ready(&mut w, b"age_l");
        let r = text(&call(
            &mut w,
            &[b"IDX.QUERY", b"age_l", b"RANGE", b"0", b"1000", b"LIMIT", b"10000"],
        ));
        let missing: Vec<u32> = (0..8000).filter(|i| !r.contains(&format!("user:{i}\r"))).collect();
        assert!(missing.is_empty(), "global_first={global_first}: missing {missing:?}");
        let v = verified(&mut w, b"age_l");
        assert_eq!((v["drift"], v["missing"]), (0, 0), "global_first={global_first}: {v:?}");
    }
}

#[test]
fn a_global_index_answers_what_a_local_one_does_and_sees_a_write_at_once() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    create(&mut w, b"age_l", &[]);
    create(
        &mut w,
        b"age_g",
        &[b"PARTITION", b"global", b"SPLIT", b"25", b"SPLIT", b"50", b"SPLIT", b"75"],
    );
    ready(&mut w, &[b"IDX.QUERY", b"age_g", b"RANGE", b"0", b"0", b"LIMIT", b"1"]);
    for i in 0..120u32 {
        let (key, age) = (format!("user:{i}"), ((i * 7) % 100).to_string());
        call(&mut w, &[b"HSET", key.as_bytes(), b"age", age.as_bytes()]);
        // the reply waited for the entry's partition owner: no sleep here
        let hit = call(
            &mut w,
            &[b"IDX.QUERY", b"age_g", b"RANGE", age.as_bytes(), age.as_bytes(), b"LIMIT", b"200"],
        );
        assert!(text(&hit).contains(&key), "{key} age {age} not visible at once: {}", text(&hit));
    }
    let all = |w: &mut Wire, name: &[u8]| {
        call(w, &[b"IDX.QUERY", name, b"RANGE", b"0", b"100", b"LIMIT", b"1000"])
    };
    assert_eq!(text(&all(&mut w, b"age_g")), text(&all(&mut w, b"age_l")));

    // a write that moves the entry to another partition
    call(&mut w, &[b"HSET", b"user:3", b"age", b"99"]);
    let moved = call(&mut w, &[b"IDX.QUERY", b"age_g", b"RANGE", b"99", b"99", b"LIMIT", b"200"]);
    assert!(text(&moved).contains("user:3"));
    let old = call(&mut w, &[b"IDX.QUERY", b"age_g", b"RANGE", b"21", b"21", b"LIMIT", b"200"]);
    assert!(!text(&old).contains("user:3"), "the old partition still holds it: {}", text(&old));
    // and one that removes it
    call(&mut w, &[b"DEL", b"user:3"]);
    let gone = call(&mut w, &[b"IDX.QUERY", b"age_g", b"RANGE", b"99", b"99", b"LIMIT", b"200"]);
    assert!(!text(&gone).contains("user:3"));
    assert_eq!(text(&all(&mut w, b"age_g")), text(&all(&mut w, b"age_l")));
}

#[test]
fn a_global_index_describes_its_splits_and_refuses_more_than_the_shards_hold() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    create(&mut w, b"g", &[b"PARTITION", b"global", b"SPLIT", b"10"]);
    let d = text(&call(&mut w, &[b"IDX.DESCRIBE", b"g"]));
    assert!(d.contains("PARTITION") && d.contains("global") && d.contains("SPLIT"), "{d}");
    let too_many: Vec<&[u8]> = vec![
        b"PARTITION",
        b"global",
        b"SPLIT",
        b"1",
        b"SPLIT",
        b"2",
        b"SPLIT",
        b"3",
        b"SPLIT",
        b"4",
    ];
    let mut q: Vec<&[u8]> = vec![
        b"IDX.CREATE",
        b"g5",
        b"ON",
        b"PREFIX",
        b"user:",
        b"FIELD",
        b"age",
        b"TYPE",
        b"i64",
        b"KIND",
        b"range",
    ];
    q.extend_from_slice(&too_many);
    assert!(
        text(&call(&mut w, &q))
            .contains("SPLIT allows at most one point fewer than the shard count")
    );
}

/// 200 rows over four partitions, a global index and a local one over the
/// same field, both storing `name` (and `age`, the local one's FIELDS
/// read the row, the global one's the stored copy).
fn seeded() -> (Server, Wire) {
    let srv = Server::start(4);
    let mut w = srv.wire();
    let values: &[&[u8]] = &[b"VALUES", b"name", b"TYPES", b"str"];
    create(&mut w, b"age_l", values);
    let mut g = values.to_vec();
    g.extend_from_slice(&[
        b"PARTITION",
        b"global",
        b"SPLIT",
        b"25",
        b"SPLIT",
        b"50",
        b"SPLIT",
        b"75",
    ]);
    create(&mut w, b"age_g", &g);
    ready(&mut w, &[b"IDX.QUERY", b"age_g", b"RANGE", b"0", b"0", b"LIMIT", b"1"]);
    ready(&mut w, &[b"IDX.QUERY", b"age_l", b"RANGE", b"0", b"0", b"LIMIT", b"1"]);
    for i in 0..200u32 {
        // partition 1 (25..50) stays sparse, so pages cross it quickly
        let age = match i % 10 {
            0 => 30 + i % 5,
            _ => (i * 13) % 100,
        };
        let (key, age, name) = (format!("user:{i}"), age.to_string(), format!("n{i}"));
        call(&mut w, &[b"HSET", key.as_bytes(), b"age", age.as_bytes(), b"name", name.as_bytes()]);
    }
    (srv, w)
}

/// Every page of `query` (with `LIMIT` and the cursor appended), in order.
fn pages(w: &mut Wire, name: &[u8], shape: &[&[u8]], tail: &[&[u8]]) -> Vec<String> {
    let mut cursor = b"0".to_vec();
    let mut out = Vec::new();
    for _ in 0..200 {
        let mut q: Vec<&[u8]> = vec![b"IDX.QUERY", name];
        q.extend_from_slice(shape);
        q.extend_from_slice(&[b"LIMIT", b"7"]);
        if cursor != b"0" {
            q.extend_from_slice(&[b"CURSOR", &cursor]);
        }
        q.extend_from_slice(tail);
        let r = call(w, &q);
        // *2 then $<len>\r\n<cursor>\r\n
        let head = text(&r);
        let next = head.split("\r\n").nth(2).expect("a cursor").as_bytes().to_vec();
        out.push(head.splitn(4, "\r\n").nth(3).unwrap_or_default().to_string());
        if next == b"0" {
            return out;
        }
        cursor = next;
    }
    panic!("the pages never ended");
}

#[test]
fn a_global_index_pages_in_the_order_a_local_one_does() {
    let (_srv, mut w) = seeded();
    for shape in [
        &[&b"RANGE"[..], b"0", b"100"][..],
        &[b"RANGE", b"20", b"60"],
        &[b"RANGE", b"26", b"49"],
        &[b"EQ", b"31"],
    ] {
        for tail in [&[][..], &[&b"FIELDS"[..], b"name"]] {
            let (g, l) =
                (pages(&mut w, b"age_g", shape, tail), pages(&mut w, b"age_l", shape, tail));
            assert!(g.len() > 1 || shape[0] == b"EQ", "{shape:?} fits one page: nothing crossed");
            assert_eq!(g, l, "{shape:?} {tail:?}");
        }
    }
    let filtered: &[&[u8]] = &[b"FILTER", b"name", b"RANGE", b"n1", b"n5"];
    let range: &[&[u8]] = &[b"RANGE", b"0", b"100"];
    assert_eq!(pages(&mut w, b"age_g", range, filtered), pages(&mut w, b"age_l", range, filtered));
}

#[test]
fn counts_and_selections_meet_every_partition_in_range() {
    let (_srv, mut w) = seeded();
    for (lo, hi) in [("0", "100"), ("20", "60"), ("26", "49"), ("80", "10")] {
        let count = |w: &mut Wire, name: &[u8]| {
            call(w, &[b"IDX.COUNT", name, b"RANGE", lo.as_bytes(), hi.as_bytes()])
        };
        assert_eq!(count(&mut w, b"age_g"), count(&mut w, b"age_l"), "{lo}..{hi}");
        let sorted = |w: &mut Wire, name: &[u8]| {
            let q: &[&[u8]] = &[
                b"IDX.QUERY",
                name,
                b"RANGE",
                lo.as_bytes(),
                hi.as_bytes(),
                b"LIMIT",
                b"15",
                b"SORT",
                b"name",
                b"DESC",
                b"FIELDS",
                b"name",
            ];
            text(&call(w, q))
        };
        assert_eq!(sorted(&mut w, b"age_g"), sorted(&mut w, b"age_l"), "{lo}..{hi}");
    }
}

#[test]
fn a_global_index_answers_fields_from_its_values_and_refuses_the_rest() {
    let (_srv, mut w) = seeded();
    let q = |name: &'static [u8], f: &'static [u8]| -> Vec<&'static [u8]> {
        vec![b"IDX.QUERY", name, b"RANGE", b"0", b"100", b"LIMIT", b"3", b"FIELDS", f]
    };
    let (g, l) = (call(&mut w, &q(b"age_g", b"name")), call(&mut w, &q(b"age_l", b"name")));
    assert_eq!(text(&g), text(&l), "the stored copy answers what the row does");
    assert!(text(&g).contains("$4\r\nname"), "{}", text(&g));
    let refused = text(&call(&mut w, &q(b"age_g", b"age")));
    assert!(
        refused.contains("FIELDS on a global index names field 'age'")
            && refused.contains("stores: name"),
        "{refused}"
    );
    let plan = text(&call(&mut w, &[b"IDX.EXPLAIN", b"age_g", b"RANGE", b"30", b"60"]));
    assert!(plan.contains("partition(s) 1..=2 of 4"), "{plan}");
}

#[test]
fn a_global_index_over_existing_rows_answers_all_of_them_or_says_it_is_building() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    // enough rows that each shard's backfill takes several ticks
    for chunk in 0..40u32 {
        let mut q: Vec<Vec<u8>> = vec![b"MSET".to_vec()];
        for i in chunk * 500..(chunk + 1) * 500 {
            q.push(format!("pad:{i}").into_bytes());
            q.push(b"x".to_vec());
        }
        let argv: Vec<&[u8]> = q.iter().map(Vec::as_slice).collect();
        call(&mut w, &argv);
    }
    for i in 0..20_000u32 {
        let (key, age) = (format!("user:{i}"), (i % 100).to_string());
        call(&mut w, &[b"HSET", key.as_bytes(), b"age", age.as_bytes()]);
    }
    create(&mut w, b"age_g", &[b"PARTITION", b"global", b"SPLIT", b"25", b"SPLIT", b"50"]);
    let count = |w: &mut Wire| call(w, &[b"IDX.COUNT", b"age_g", b"RANGE", b"0", b"100"]);
    let (mut building, mut answers) = (0, 0);
    while answers < 20 {
        let r = count(&mut w);
        if r.starts_with(b"-INDEXBUILDING") {
            building += 1;
            continue;
        }
        assert_eq!(text(&r), ":20000\r\n", "an answer before every shard's rows arrived");
        answers += 1;
    }
    assert!(building > 0, "the build finished before the first query: nothing was tested");
}

/// The split values of `name`'s `partitioning` pair in `IDX.DESCRIBE`, as
/// integers (empty for a local index or a global one with one partition).
fn described_splits(w: &mut Wire, name: &[u8]) -> Vec<i64> {
    let d = text(&call(w, &[b"IDX.DESCRIBE", name]));
    let Some(at) = d.find("$6\r\nglobal\r\n*") else { return Vec::new() };
    let lines: Vec<&str> = d[at..].split("\r\n").collect();
    let n: usize = lines[2][1..].parse().unwrap();
    (0..n).map(|i| lines[4 + 2 * i].parse().unwrap()).collect()
}

/// Every partition answers: a range over the whole domain reaches all of
/// them, where a narrow one reaches one and can pass while the others build.
fn wait_ready(w: &mut Wire, name: &[u8]) {
    let r =
        ready(w, &[b"IDX.COUNT", name, b"RANGE", b"-9223372036854775808", b"9223372036854775807"]);
    assert!(r.starts_with(b":"), "not a count: {}", text(&r));
}

#[test]
fn a_global_index_without_split_points_samples_them_and_a_rebuild_resamples() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    for i in 0..8000u32 {
        let (key, age) = (format!("user:{i}"), ((i * 7919) % 1000).to_string());
        call(&mut w, &[b"HSET", key.as_bytes(), b"age", age.as_bytes()]);
    }
    create(&mut w, b"age_g", &[b"PARTITION", b"global"]);
    create(&mut w, b"age_l", &[]);
    let splits = described_splits(&mut w, b"age_g");
    assert_eq!(splits.len(), 3, "the shard count's quartiles: {splits:?}");
    for (s, want) in splits.iter().zip([250, 500, 750]) {
        assert!((s - want).abs() < 60, "split {s}, expected near {want}");
    }
    wait_ready(&mut w, b"age_g");
    wait_ready(&mut w, b"age_l");
    let all = |w: &mut Wire, name: &[u8]| {
        text(&call(w, &[b"IDX.QUERY", name, b"RANGE", b"0", b"1000", b"LIMIT", b"10000"]))
    };
    assert_eq!(all(&mut w, b"age_g"), all(&mut w, b"age_l"));

    // created over nothing: one partition, until a rebuild samples the rows
    call(&mut w, &[b"FLUSHALL"]);
    create(&mut w, b"late", &[b"PARTITION", b"global"]);
    assert!(described_splits(&mut w, b"late").is_empty());
    for i in 0..8000u32 {
        let (key, age) = (format!("user:{i}"), (i % 400).to_string());
        call(&mut w, &[b"HSET", key.as_bytes(), b"age", age.as_bytes()]);
    }
    assert_eq!(call(&mut w, &[b"IDX.REBUILD", b"late"]), b"+OK\r\n");
    let splits = described_splits(&mut w, b"late");
    assert_eq!(splits.len(), 3, "{splits:?}");
    wait_ready(&mut w, b"late");
    assert_eq!(all(&mut w, b"late"), all(&mut w, b"age_l"));
}

#[test]
fn idx_list_shows_how_a_global_index_is_spread() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    create(&mut w, b"age_l", &[]);
    create(&mut w, b"age_g", &[b"PARTITION", b"global", b"SPLIT", b"50"]);
    for i in 0..100u32 {
        // 70 below the split, 30 above
        let (key, age) = (format!("user:{i}"), if i < 70 { "10" } else { "90" });
        call(&mut w, &[b"HSET", key.as_bytes(), b"age", age.as_bytes()]);
    }
    wait_ready(&mut w, b"age_g");
    let list = text(&call(&mut w, &[b"IDX.LIST"]));
    let row = |name: &str| {
        let at = list.find(&format!("\r\n{name}\r\n")).unwrap();
        list[at..].split("*").next().unwrap().to_string()
    };
    let (g, l) = (row("age_g"), row("age_l"));
    assert!(l.contains("partitioning\r\n$5\r\nlocal") && !l.contains("max_entries"), "{l}");
    for pair in [
        "partitioning\r\n$6\r\nglobal",
        "partitions\r\n$1\r\n2",
        "max_entries\r\n$2\r\n70",
        "mean_entries\r\n$4\r\n50.0",
        "entries\r\n$3\r\n100",
    ] {
        assert!(g.contains(pair), "{pair} in {g}");
    }
}

/// VERIFY's counters by label.
fn verified(w: &mut Wire, name: &[u8]) -> std::collections::HashMap<String, u64> {
    let r = text(&call(w, &[b"IDX.VERIFY", name]));
    let words: Vec<&str> = r.split("\r\n").filter(|s| !s.starts_with(['*', '$'])).collect();
    words.chunks(2).filter_map(|kv| Some((kv[0].to_string(), kv.get(1)?.parse().ok()?))).collect()
}

#[test]
fn verify_on_a_global_index_matches_every_row_to_its_entry() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    for i in 0..600u32 {
        let key = format!("user:{i}");
        // every 50th row cannot be indexed; every 100th shares its email
        let age = if i % 50 == 7 { "old".to_string() } else { (i % 90).to_string() };
        let email = if i % 100 == 0 { "shared@x".to_string() } else { format!("u{i}@x") };
        call(
            &mut w,
            &[b"HSET", key.as_bytes(), b"age", age.as_bytes(), b"email", email.as_bytes()],
        );
    }
    create(&mut w, b"age_l", &[]);
    create(&mut w, b"age_g", &[b"PARTITION", b"global"]);
    let unique: &[&[u8]] = &[
        b"IDX.CREATE",
        b"email_g",
        b"ON",
        b"PREFIX",
        b"user:",
        b"FIELD",
        b"email",
        b"TYPE",
        b"str",
        b"KIND",
        b"unique",
        b"PARTITION",
        b"global",
    ];
    assert_eq!(call(&mut w, unique), b"+OK\r\n");
    for name in [&b"age_l"[..], b"age_g", b"email_g"] {
        wait_ready(&mut w, name);
    }
    let (g, l) = (verified(&mut w, b"age_g"), verified(&mut w, b"age_l"));
    assert_eq!((g["drift"], g["missing"]), (0, 0), "{g:?}");
    for label in ["entries", "coerce_failures", "checked"] {
        assert_eq!(g[label], l[label], "{label}: global {g:?} local {l:?}");
    }
    assert_eq!(g["entries"], 588);
    // six rows share one email: one value held by more than one key
    let u = verified(&mut w, b"email_g");
    assert_eq!((u["entries"], u["duplicates"], u["drift"]), (600, 1, 0), "{u:?}");
}

fn words(line: &str) -> Vec<Vec<u8>> {
    line.split(' ').map(|w| w.as_bytes().to_vec()).collect()
}

fn run(w: &mut Wire, line: &str) -> String {
    let argv = words(line);
    let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
    text(&call(w, &refs))
}

const DECLARE: &str = "TABLE.DECLARE u PREFIX user: PK id COLUMN id i64 COLUMN age i64 \
    COLUMN city str INDEX age range GLOBAL SPLIT AT 30 60 ORDERPATH by_city ON city THEN age GLOBAL";

#[test]
fn a_table_declares_its_paths_global_and_reads_the_declaration_back() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    for i in 0..300u32 {
        let (key, age, city) = (format!("user:{i}"), (i % 90).to_string(), format!("c{}", i % 7));
        call(
            &mut w,
            &[
                b"HSET",
                key.as_bytes(),
                b"id",
                &key.as_bytes()[5..],
                b"age",
                age.as_bytes(),
                b"city",
                city.as_bytes(),
            ],
        );
    }
    assert_eq!(run(&mut w, DECLARE), "+OK\r\n");
    let age = run(&mut w, "IDX.DESCRIBE u.age");
    assert!(age.contains("$6\r\nglobal\r\n*2\r\n$2\r\n30\r\n$2\r\n60"), "{age}");
    assert!(run(&mut w, "IDX.DESCRIBE u.by_city").contains("global"));
    create(&mut w, b"age_l", &[]);
    wait_ready(&mut w, b"u.age");
    wait_ready(&mut w, b"age_l");
    assert_eq!(
        run(&mut w, "IDX.QUERY u.age RANGE 20 70 LIMIT 500"),
        run(&mut w, "IDX.QUERY age_l RANGE 20 70 LIMIT 500")
    );

    // the declaration read back recreates the same table
    let table = run(&mut w, "TABLE.DESCRIBE u");
    assert!(table.contains("GLOBAL\r\n$5\r\nSPLIT\r\n$2\r\nAT\r\n$2\r\n30\r\n$2\r\n60"), "{table}");
    assert!(run(&mut w, DECLARE).starts_with("-ERR"), "declared twice");
    assert_eq!(run(&mut w, DECLARE.replacen("DECLARE", "ENSURE", 1).as_str()), "+UNCHANGED\r\n");
    let local = DECLARE.replace(" GLOBAL SPLIT AT 30 60", "");
    assert!(run(&mut w, &local.replacen("DECLARE", "ENSURE", 1)).contains("spread differently"));
    assert!(table.contains("GLOBAL\r\n$5\r\nSPLIT\r\n$2\r\nAT\r\n$"), "orderpath splits: {table}");
    assert!(table.matches("0x").count() >= 3, "the sampled orderpath splits, in hex: {table}");
    assert_eq!(run(&mut w, "TABLE.DROP u"), ":1\r\n");
    let replay = described_declaration(&table);
    let refs: Vec<&[u8]> = replay.iter().map(Vec::as_slice).collect();
    assert_eq!(text(&call(&mut w, &refs)), "+OK\r\n");
    assert_eq!(run(&mut w, "TABLE.DESCRIBE u"), table, "the replay recreates the same table");
}

/// The `declaration` argv of a TABLE.DESCRIBE reply.
fn described_declaration(reply: &str) -> Vec<Vec<u8>> {
    let lines: Vec<&str> = reply.split("\r\n").collect();
    let at = lines.iter().position(|l| *l == "declaration").expect("a declaration");
    let n: usize = lines[at + 1][1..].parse().unwrap();
    (0..n).map(|i| lines[at + 3 + 2 * i].as_bytes().to_vec()).collect()
}

#[test]
fn a_global_path_is_refused_by_name_where_it_cannot_apply() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    let base = "TABLE.DECLARE e PREFIX ev: PK id COLUMN id i64 COLUMN at i64";
    let windowed =
        run(&mut w, &format!("{base} INDEX at range GLOBAL WINDOW at SPAN 50 BUCKET 10"));
    assert!(windowed.contains("GLOBAL cannot apply to a windowed table"), "{windowed}");
    let op = run(&mut w, &format!("{base} ORDERPATH o ON at GLOBAL SPLIT AT 5"));
    assert!(op.contains("ORDERPATH's SPLIT AT values are its encoded order bytes"), "{op}");
    let many = run(&mut w, &format!("{base} INDEX at range GLOBAL SPLIT AT 1 2 3 4"));
    assert!(many.contains("at most one point fewer than the shard count"), "{many}");
    let bad = run(&mut w, &format!("{base} INDEX at range GLOBAL SPLIT AT x"));
    assert!(bad.contains("does not coerce"), "{bad}");
    assert!(run(&mut w, "TABLE.LIST").starts_with("*0"), "nothing was admitted");
}

/// `(max_entries, mean_entries)` of `name` from `IDX.LIST`.
fn spread(w: &mut Wire, name: &str) -> (f64, f64) {
    let list = text(&call(w, &[b"IDX.LIST"]));
    let at = list.find(&format!("\r\n{name}\r\n")).expect("listed");
    let row: Vec<&str> = list[at..].split("\r\n").filter(|s| !s.starts_with(['*', '$'])).collect();
    let get = |k: &str| row.iter().position(|s| *s == k).map(|i| row[i + 1].parse().unwrap());
    (get("max_entries").unwrap(), get("mean_entries").unwrap())
}

fn load(w: &mut Wire, from: u32, to: u32, value: impl Fn(u32) -> u32) {
    for i in from..to {
        let (key, v) = (format!("user:{i}"), value(i).to_string());
        call(w, &[b"HSET", key.as_bytes(), b"age", v.as_bytes()]);
    }
}

#[test]
fn sampled_partitions_start_even_drift_shows_and_a_rebuild_evens_them_again() {
    let srv = Server::start(16);
    let mut w = srv.wire();
    // uniform over 0..1_000_000, scrambled against the key order
    load(&mut w, 0, 40_000, |i| (i.wrapping_mul(2_654_435_761) >> 8) % 1_000_000);
    create(&mut w, b"g", &[b"PARTITION", b"global"]);
    wait_ready(&mut w, b"g");
    // every shard sends its values in 256 rank buckets per partition, so a
    // split is off by at most 1/256 of a partition and a partition by 2/256
    let (max, mean) = spread(&mut w, "g");
    eprintln!("uniform, N=16: max/mean {:.3}", max / mean);
    assert!(max / mean <= 1.0 + 2.0 / 256.0, "uniform: {max} / {mean}");
    // append-only drift: new rows all above the old largest value
    load(&mut w, 40_000, 60_000, |i| 1_000_000 + i);
    let (max, mean) = spread(&mut w, "g");
    eprintln!("after drift: max/mean {:.3}", max / mean);
    assert!(max / mean > 4.0, "the last partition took every new row: {max} / {mean}");
    assert_eq!(call(&mut w, &[b"IDX.REBUILD", b"g"]), b"+OK\r\n");
    wait_ready(&mut w, b"g");
    let (max, mean) = spread(&mut w, "g");
    eprintln!("rebuilt: max/mean {:.3}", max / mean);
    assert!(max / mean <= 1.0 + 2.0 / 256.0, "rebuilt: {max} / {mean}");
}

#[test]
fn a_skewed_domain_is_as_even_as_its_heaviest_value_allows() {
    let srv = Server::start(16);
    let mut w = srv.wire();
    // Zipf-like: value v held by about 1/v of the rows
    let zipf = |i: u32| {
        let u = (i.wrapping_mul(2_654_435_761) >> 8) % 1_000_000;
        (1_000_000 / (u + 1)).min(100_000)
    };
    load(&mut w, 0, 40_000, zipf);
    let heaviest = (0..40_000u32).filter(|&i| zipf(i) == 1).count() as f64;
    create(&mut w, b"z", &[b"PARTITION", b"global"]);
    wait_ready(&mut w, b"z");
    let (max, mean) = spread(&mut w, "z");
    eprintln!("zipf, N=16: max/mean {:.3}, heaviest value {heaviest} rows", max / mean);
    assert!(max <= (heaviest.max(mean) + 0.1 * mean).ceil(), "{max} / {mean}, heaviest {heaviest}");
}

#[test]
fn eq_on_a_global_unique_index_finds_every_holder_of_the_value() {
    let srv = Server::start(8);
    let mut w = srv.wire();
    let unique: &[&[u8]] = &[
        b"IDX.CREATE",
        b"email",
        b"ON",
        b"PREFIX",
        b"user:",
        b"FIELD",
        b"email",
        b"TYPE",
        b"str",
        b"KIND",
        b"unique",
        b"PARTITION",
        b"global",
    ];
    for i in 0..400u32 {
        let (key, email) = (format!("user:{i}"), format!("e{}", i % 50));
        call(&mut w, &[b"HSET", key.as_bytes(), b"email", email.as_bytes()]);
    }
    assert_eq!(call(&mut w, unique), b"+OK\r\n");
    wait_ready(&mut w, b"email");
    let holders = text(&call(&mut w, &[b"IDX.QUERY", b"email", b"EQ", b"e7", b"LIMIT", b"100"]));
    let keys: Vec<&str> = holders.split("\r\n").filter(|s| s.starts_with("user:")).collect();
    assert_eq!(keys.len(), 8, "every key whose email is e7, from one partition: {holders}");
    let v = verified(&mut w, b"email");
    assert_eq!(v["duplicates"], 50, "{v:?}");
}

#[test]
fn every_shard_sends_its_rank_buckets_and_the_partitions_start_even() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    // 12,000 rows, about 3,000 a shard: 1,024 buckets each, of about 3 rows
    load(&mut w, 0, 12_000, |i| (i.wrapping_mul(2_654_435_761) >> 8) % 1_000_000);
    create(&mut w, b"g", &[b"PARTITION", b"global"]);
    wait_ready(&mut w, b"g");
    let (max, mean) = spread(&mut w, "g");
    eprintln!("uniform, N=4: max/mean {:.3}", max / mean);
    assert!(max / mean <= 1.0 + 2.0 / 256.0, "{max} / {mean}");
}

#[test]
fn a_sampled_global_index_created_inside_multi() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    load(&mut w, 0, 2_000, |i| i % 100);
    assert_eq!(run(&mut w, "MULTI"), "+OK\r\n");
    let create = "IDX.CREATE g ON PREFIX user: FIELD age TYPE i64 KIND range PARTITION global";
    assert_eq!(run(&mut w, create), "+QUEUED\r\n");
    assert_eq!(run(&mut w, "EXEC"), "*1\r\n+OK\r\n");
    wait_ready(&mut w, b"g");
    assert_eq!(described_splits(&mut w, b"g"), [25, 50, 75]);
}

/// `(entries, bytes)` of `name` from `IDX.LIST`.
fn listed_size(w: &mut Wire, name: &str) -> (u64, u64) {
    let list = text(&call(w, &[b"IDX.LIST"]));
    let at = list.find(&format!("\r\n{name}\r\n")).expect("listed");
    let row: Vec<&str> = list[at..].split("\r\n").filter(|s| !s.starts_with(['*', '$'])).collect();
    let get = |k: &str| row.iter().position(|s| *s == k).map(|i| row[i + 1].parse().unwrap());
    (get("entries").unwrap(), get("bytes").unwrap())
}

#[test]
fn a_global_index_holds_no_more_than_a_local_one() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    create(&mut w, b"age_l", &[]);
    create(
        &mut w,
        b"age_g",
        &[b"PARTITION", b"global", b"SPLIT", b"25", b"SPLIT", b"50", b"SPLIT", b"75"],
    );
    load(&mut w, 0, 20_000, |i| i % 100);
    let (le, lb) = listed_size(&mut w, "age_l");
    let (ge, gb) = listed_size(&mut w, "age_g");
    assert_eq!((le, ge), (20_000, 20_000));
    let per_row = |b: u64| b as f64 / 20_000.0;
    eprintln!("bytes per row: local {:.1}, global {:.1}", per_row(lb), per_row(gb));
    // the partitions hold the same entries, and a row's shard keeps no map
    // to its partition: a write names the old value, which names it
    assert!(gb as f64 <= lb as f64 * 1.1, "local {lb}, global {gb}");
}

#[test]
fn partition_options_that_cannot_apply_are_refused_by_name() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    let base = "IDX.CREATE x ON PREFIX user: FIELD age TYPE i64 KIND range";
    for (tail, why) in [
        ("PARTITION sideways", "PARTITION must be local|global"),
        ("SPLIT 5", "SPLIT requires PARTITION global"),
        ("PARTITION global SPLIT five", "does not coerce to the index TYPE"),
    ] {
        let r = run(&mut w, &format!("{base} {tail}"));
        assert!(r.contains(why), "{tail}: {r}");
    }
    assert_eq!(run(&mut w, &format!("{base} PARTITION local")), "+OK\r\n");
    assert!(run(&mut w, "IDX.DESCRIBE x").contains("partitioning\r\n$5\r\nlocal"));
    // a selection reaches every partition met, and the plan says so
    create(
        &mut w,
        b"g",
        &[b"PARTITION", b"global", b"SPLIT", b"50", b"VALUES", b"age", b"TYPES", b"i64"],
    );
    let plan = run(&mut w, "IDX.EXPLAIN g RANGE 0 100 SORT age DESC");
    assert!(plan.contains("partition(s) 0..=1 of 2, each partition met answers"), "{plan}");
}

#[test]
fn a_table_replaced_with_a_sampled_global_path_samples_every_shard() {
    let srv = Server::start(4);
    let mut w = srv.wire();
    load(&mut w, 0, 8_000, |i| (i * 7919) % 1000);
    let declare = "TABLE.DECLARE u PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range";
    assert_eq!(run(&mut w, declare), "+OK\r\n");
    let replace = format!("{} GLOBAL", declare.replacen("DECLARE", "REPLACE", 1));
    assert_eq!(run(&mut w, &replace), "+OK\r\n");
    let splits = described_splits(&mut w, b"u.age");
    assert_eq!(splits.len(), 3, "{splits:?}");
    for (s, want) in splits.iter().zip([250, 500, 750]) {
        assert!((s - want).abs() < 60, "split {s}, expected near {want}");
    }
}
