//! crashgate's windowed victim: a durable store with a fast reaper, a
//! windowed table and a text index, written forever until SIGKILLed.
//! Every row advances the window column, so the slide machinery —
//! seal, manifest, SEGMENTED frame, phase change, index cut, text
//! freeze — runs continuously and the kill lands inside whichever gap
//! it lands in. Emits `SYNCED <n>` after every explicit fsync
//! barrier: the loss bound is "every row at or before the last SYNCED
//! line survives the kill, wherever it lives (hot or segment)".
//!
//!   crash_window_writer <dir> [--shards N] [--always]
use std::io::Write as _;
use std::time::Duration;

use kevy_embedded::{AppendFsync, Config, Store};
use kevy_index::{IndexKind, TableIndex, TableSpec, ValType, WindowSpec};

const SYNC_EVERY: u64 = 200;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: crash_window_writer <dir> [flags]");
    let mut shards = 1usize;
    let mut fsync = AppendFsync::EverySec;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--shards" => shards = args.next().unwrap().parse().unwrap(),
            "--always" => fsync = AppendFsync::Always,
            other => panic!("unknown flag {other}"),
        }
    }
    let store = Store::open(
        Config::default()
            .with_persist(&dir)
            .with_shards(shards)
            .with_appendfsync(fsync)
            .with_reaper_interval(Duration::from_millis(5)),
    )
    .expect("open");

    // A windowed table (span 50, bucket 10 — every ~10 rows slide a
    // bucket out) plus a text index riding the same rows, so the kill
    // can land mid-freeze too. Idempotent under replay: the catalog
    // survives restarts, re-declaring identically is a no-op.
    let spec = TableSpec {
        name: b"ev".to_vec(),
        prefix: b"r:".to_vec(),
        pk: b"id".to_vec(),
        columns: vec![(b"id".to_vec(), ValType::Str), (b"at".to_vec(), ValType::I64)],
        indexes: vec![TableIndex {
            column: b"at".to_vec(),
            kind: IndexKind::Range,
            values: vec![],
        }],
        orderpaths: vec![],
        window: Some(WindowSpec { column: b"at".to_vec(), span: 50, bucket: 10 }),
        autodeclare: 0,
        auto_added: vec![],
    };
    store.table_declare(spec).expect("declare");
    store
        .idx_create_text(b"ev.note", b"r:", &[(b"note", 1.0)], false, &[])
        .expect("text index");

    let mut out = std::io::stdout().lock();
    for n in 1u64.. {
        let key = format!("r:{n}");
        let owned: Vec<Vec<u8>> = vec![
            b"HSET".to_vec(),
            key.clone().into_bytes(),
            b"id".to_vec(),
            key.into_bytes(),
            b"at".to_vec(),
            n.to_string().into_bytes(),
            b"note".to_vec(),
            format!("event number {n} of the crash run").into_bytes(),
        ];
        let mut reply = Vec::new();
        store.dispatch_argv(&owned, &mut reply);
        assert!(!reply.starts_with(b"-ERR"), "{}", String::from_utf8_lossy(&reply));
        if n % SYNC_EVERY == 0 {
            store.fsync_aof().expect("fsync");
            writeln!(out, "SYNCED {n}").unwrap();
            out.flush().unwrap();
        }
    }
}
