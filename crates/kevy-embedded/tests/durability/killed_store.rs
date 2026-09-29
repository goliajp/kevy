//! A store whose process is killed keeps every write that returned, and
//! the next open — whatever it is configured to do — gets them back.
//! `mem::forget` stands in for the kill: no close runs, and with the reaper
//! in manual mode no background thread drains anything in the meantime.

#![cfg(feature = "persist")]

use kevy_embedded::{AppendFsync, Config, Store};

fn config(dir: &std::path::Path) -> Config {
    Config::default()
        .with_persist(dir)
        .with_appendfsync(AppendFsync::EverySec)
        .with_ttl_reaper_manual()
}

fn write_and_kill(cfg: Config, keys: std::ops::Range<u32>) {
    let dir = cfg.data_dir.clone().expect("a persistent store");
    let s = Store::open(cfg).expect("open");
    for i in keys {
        s.set(format!("k{i}").as_bytes(), format!("v{i}").as_bytes()).expect("set");
    }
    std::mem::forget(s);
    // a dead process's lock goes with it; here the forgotten store still
    // holds it, on a file the next open no longer finds
    std::fs::remove_file(dir.join("LOCK")).expect("the directory lock file");
}

fn assert_all(s: &Store, keys: std::ops::Range<u32>) {
    for i in keys {
        let got = s.get(format!("k{i}").as_bytes()).expect("get");
        assert_eq!(got.as_deref(), Some(format!("v{i}").as_bytes()), "k{i}");
    }
}

#[test]
fn staged_writes_come_back_after_a_kill() {
    let dir = kevy_tmpdir::TmpDir::new("killed-ring");
    write_and_kill(config(dir.path()).with_stage_ring(64 * 1024).with_mapped_aof(false), 0..300);
    let s = Store::open(config(dir.path()).with_mapped_aof(false)).expect("reopen");
    assert!(s.open_report().stage_recovered > 0, "the ring owed the log these writes");
    assert_all(&s, 0..300);
}

#[test]
fn mapped_writes_come_back_after_a_kill() {
    let dir = kevy_tmpdir::TmpDir::new("killed-mapped");
    write_and_kill(config(dir.path()).with_mapped_aof(true), 0..300);
    let s = Store::open(config(dir.path()).with_mapped_aof(true)).expect("reopen");
    assert_all(&s, 0..300);
    drop(s);
    let s = Store::open(config(dir.path()).with_mapped_aof(false)).expect("reopen through write()");
    assert_all(&s, 0..300);
}

#[test]
fn a_store_that_stops_staging_still_collects_what_its_ring_owed() {
    let dir = kevy_tmpdir::TmpDir::new("killed-unstaged");
    write_and_kill(config(dir.path()).with_stage_ring(64 * 1024).with_mapped_aof(false), 0..200);
    let s =
        Store::open(config(dir.path()).with_stage_ring(0).with_mapped_aof(false)).expect("reopen");
    assert!(s.open_report().stage_recovered > 0);
    assert_all(&s, 0..200);
    assert!(
        !dir.path().join("aof-0.aof.stage").exists(),
        "a store that does not stage keeps no ring"
    );
}

#[test]
fn a_reshard_after_a_kill_carries_what_the_ring_owed() {
    let dir = kevy_tmpdir::TmpDir::new("killed-reshard");
    write_and_kill(config(dir.path()).with_stage_ring(64 * 1024).with_mapped_aof(false), 0..400);
    let s = Store::open(config(dir.path()).with_shards(4).with_mapped_aof(false)).expect("reshard");
    assert_all(&s, 0..400);
    drop(s);
    let s = Store::open(config(dir.path()).with_shards(4).with_mapped_aof(false)).expect("reopen");
    assert_all(&s, 0..400);
}

#[test]
#[should_panic(expected = "a staging ring is 0 or a power of two of at least 64 KiB")]
fn a_ring_size_that_cannot_work_is_refused() {
    let _ = Config::default().with_stage_ring(100_000);
}

fn call(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut reply = Vec::new();
    s.dispatch_argv(&owned, &mut reply);
    reply
}

#[cfg(feature = "index")]
#[test]
fn a_window_eviction_the_ring_still_owed_survives_a_kill() {
    use kevy_index::{IndexKind, TableIndex, TableSpec, ValType, WindowSpec};
    let dir = kevy_tmpdir::TmpDir::new("killed-window");
    let cfg = || config(dir.path()).with_stage_ring(64 * 1024).with_mapped_aof(false);
    let s = Store::open(cfg()).expect("open");
    s.table_declare({
        let mut t = TableSpec::default();
        t.name = b"ev".to_vec();
        t.prefix = b"r:".to_vec();
        t.pk = b"id".to_vec();
        t.columns = vec![(b"id".to_vec(), ValType::Str), (b"at".to_vec(), ValType::I64)];
        t.indexes = vec![TableIndex::new(b"at".to_vec(), IndexKind::Range)];
        t.window = Some(WindowSpec::new(b"at".to_vec(), 50, 10));
        t
    })
    .expect("declare");
    for n in 1..=200u32 {
        let (key, at) = (format!("r:{n}"), n.to_string());
        let reply =
            call(&s, &[b"HSET", key.as_bytes(), b"id", key.as_bytes(), b"at", at.as_bytes()]);
        assert_eq!(reply, b":2\r\n");
    }
    // a manual tick slides the window but leaves the ring undrained: the
    // SEGMENTED frame is owed by the ring, its segment already sealed
    s.tick();
    let log = std::fs::read(dir.path().join("aof-0.aof")).expect("the log");
    assert!(
        !log.windows(9).any(|w| w == b"SEGMENTED"),
        "the frame reached the log; this test would not test the ring"
    );
    let segs = std::fs::read_dir(dir.path().join("segs-0")).expect("segments");
    assert!(segs.flatten().any(|e| e.file_name().to_string_lossy().starts_with("row-")));
    std::mem::forget(s);
    std::fs::remove_file(dir.path().join("LOCK")).expect("the directory lock file");

    // the second open, after a clean close, must not lose what the first
    // recovered
    for round in 0..2 {
        let s = Store::open(cfg()).expect("reopen");
        assert_eq!(s.open_report().stage_recovered > 0, round == 0);
        for n in 1..=200u32 {
            let key = format!("r:{n}");
            let at = n.to_string();
            let want = [format!("${}\r\n", at.len()).as_bytes(), at.as_bytes(), b"\r\n"].concat();
            assert_eq!(call(&s, &[b"HGET", key.as_bytes(), b"at"]), want, "{key}");
        }
    }
}
