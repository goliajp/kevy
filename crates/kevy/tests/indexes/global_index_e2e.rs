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
