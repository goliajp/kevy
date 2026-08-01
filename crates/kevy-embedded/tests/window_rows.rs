//! Row eviction's KV-transparency gate: after a windowed table's
//! out-of-window rows phase-change into seg-backed stubs, every KV
//! command answers identically to a control table whose rows never
//! left the hot map — reads, enumeration, TTL surfaces, and the whole
//! write-revival family (HSET must MERGE against the cold copy, never
//! replace it). TTL-bearing rows must refuse to evict.

#![cfg(all(feature = "index", feature = "persist", not(target_arch = "wasm32")))]

use std::time::{Duration, Instant};

use kevy_embedded::{Config, Store};
use kevy_index::{IndexKind, TableIndex, TableSpec, ValType, WindowSpec};

fn run(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
    out
}

fn table(name: &[u8], windowed: bool) -> TableSpec {
    TableSpec {
        name: name.to_vec(),
        prefix: b"ev:".to_vec(),
        pk: b"id".to_vec(),
        columns: vec![(b"id".to_vec(), ValType::Str), (b"at".to_vec(), ValType::I64)],
        indexes: vec![TableIndex {
            column: b"at".to_vec(),
            kind: IndexKind::Range,
            values: vec![],
        }],
        orderpaths: vec![],
        window: windowed.then_some(WindowSpec {
            column: b"at".to_vec(),
            span: 100,
            bucket: 10,
        }),
    }
}

/// Seed rows ev:0..ev:29 (at = i*10) plus a TTL-bearing cold-aged row.
fn seed(s: &Store) {
    for i in 0..30i64 {
        let key = format!("ev:{i}");
        let at = (i * 10).to_string();
        run(s, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(), b"at", at.as_bytes(),
                 b"note", format!("row number {i}").as_bytes()]);
    }
    // Out-of-window by value, but TTL'd: must stay hot.
    run(s, &[b"HSET", b"ev:ttl", b"id", b"ev:ttl", b"at", b"5", b"note", b"short-lived"]);
    run(s, &[b"EXPIRE", b"ev:ttl", b"1000"]);
}

fn wait_for_row_segment(dir: &std::path::Path) {
    let segs = dir.join("segs-0");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let slid = segs.exists()
            && std::fs::read_dir(&segs).is_ok_and(|r| {
                r.filter_map(Result::ok)
                    .any(|e| e.file_name().to_string_lossy().starts_with("row-"))
            });
        if slid {
            return;
        }
        assert!(Instant::now() < deadline, "rows never evicted");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn cold_rows_answer_every_kv_command_like_hot_ones() {
    let d = kevy_tmpdir::TmpDir::new("emb-winrows");
    let s = Store::open(
        Config::default()
            .with_persist(d.path())
            .with_reaper_interval(Duration::from_millis(25)),
    )
    .expect("open");
    s.table_declare(table(b"ev", true)).expect("declare ev");
    seed(&s);
    wait_for_row_segment(d.path());

    // A control store, same rows, never windowed.
    let dc = kevy_tmpdir::TmpDir::new("emb-winrows-ctl");
    let c = Store::open(Config::default().with_persist(dc.path())).expect("open ctl");
    c.table_declare(table(b"ev", false)).expect("declare ctl");
    seed(&c);

    let compare = |tag: &str| {
        // Read family, on a cold row (ev:2, at=20 < W=190) and a hot one.
        for key in [b"ev:2".as_slice(), b"ev:25", b"ev:ttl", b"ev:none"] {
            for cmd in [
                vec![b"HGETALL".as_slice(), key],
                vec![b"HGET", key, b"note"],
                vec![b"HGET", key, b"ghost"],
                vec![b"HMGET", key, b"id", b"ghost", b"at"],
                vec![b"HLEN", key],
                vec![b"HKEYS", key],
                vec![b"HEXISTS", key, b"at"],
                vec![b"EXISTS", key],
                vec![b"TYPE", key],
                vec![b"TTL", key],
                vec![b"HSCAN", key, b"0"],
            ] {
                let name = String::from_utf8_lossy(cmd[0]).into_owned();
                assert_eq!(
                    run(&s, &cmd),
                    run(&c, &cmd),
                    "{tag}: {name} {}",
                    String::from_utf8_lossy(key)
                );
            }
        }
        assert_eq!(run(&s, &[b"DBSIZE"]), run(&c, &[b"DBSIZE"]), "{tag}: DBSIZE");
        // SCAN: full sweep, order-insensitive key-set equality.
        let mut all_s = scan_all(&s);
        let mut all_c = scan_all(&c);
        all_s.sort();
        all_c.sort();
        assert_eq!(all_s, all_c, "{tag}: SCAN key set");
    };
    compare("after eviction");

    // The TTL row must NOT have evicted: its value is still hot.
    // (Both faces agree above; here we pin the mechanism itself.)
    let ttl_reply = run(&s, &[b"TTL", b"ev:ttl"]);
    assert!(ttl_reply.starts_with(b":") && ttl_reply != b":-1\r\n".as_slice(), "{ttl_reply:?}");

    // Write-revival family, each against the cold copy — HSET merges,
    // HSETNX respects existing cold fields, HINCRBY reads the cold
    // value, HDEL counts cold fields, DEL kills the row.
    for cmds in [
        vec![vec![b"HSET".as_slice(), b"ev:2", b"extra", b"x"]],
        vec![vec![b"HSETNX", b"ev:3", b"note", b"MUST-NOT-WIN"]],
        vec![vec![b"HSET", b"ev:4", b"n", b"7"], vec![b"HINCRBY", b"ev:4", b"n", b"5"]],
        vec![vec![b"HDEL", b"ev:5", b"note", b"ghost"]],
        vec![vec![b"DEL", b"ev:6"]],
    ] {
        for cmd in &cmds {
            let cs: Vec<&[u8]> = cmd.to_vec();
            assert_eq!(run(&s, &cs), run(&c, &cs), "write: {}", String::from_utf8_lossy(cmd[0]));
        }
    }
    compare("after revival churn");

    // The merge pin, explicitly: the revived ev:2 kept its cold fields.
    let all = String::from_utf8_lossy(&run(&s, &[b"HGETALL", b"ev:2"])).into_owned();
    for want in ["id", "at", "note", "extra", "row number 2"] {
        assert!(all.contains(want), "revived row lost '{want}': {all}");
    }
}

fn scan_all(s: &Store) -> Vec<Vec<u8>> {
    let mut keys = Vec::new();
    let mut cursor = b"0".to_vec();
    loop {
        let reply = run(s, &[b"SCAN", &cursor, b"COUNT", b"100"]);
        let text = String::from_utf8_lossy(&reply).into_owned();
        let mut lines = text.split("\r\n");
        // *2 / $n / <cursor> / *k / ($n / key)*
        lines.next();
        lines.next();
        cursor = lines.next().unwrap_or("0").as_bytes().to_vec();
        let mut prev_was_len = false;
        for l in lines {
            if l.starts_with('$') {
                prev_was_len = true;
                continue;
            }
            if prev_was_len && !l.is_empty() {
                keys.push(l.as_bytes().to_vec());
            }
            prev_was_len = false;
        }
        if cursor == b"0" {
            return keys;
        }
    }
}
