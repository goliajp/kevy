//! crashgate's victim process: open a durable store and write forever
//! until SIGKILLed. Emits `SYNCED <n>` after every explicit fsync barrier —
//! the crash-consistency contract's loss bound is "everything at or before
//! the last SYNCED line survives the kill".
//!
//!   crash_writer <dir> [--shards N] [--always] [--feed] [--rewrite] [--snapshot]
//!                      [--acked] [--ring | --mapped | --no-stage]
//!
//! Modes stack: --rewrite / --snapshot fold a background compaction /
//! snapshot into the write loop so the kill can land mid-rewrite or
//! mid-snapshot; --feed opens the CDC ring so the kill can land mid-emit.
//! --acked prints `ACKED <n>` once each write has returned: through the
//! staging ring (--ring) or a mapped log (--mapped) a process kill loses
//! none of those; --no-stage turns both off, for a control run. Without
//! any of the three the platform default applies.
use std::io::Write as _;

use kevy_embedded::{AppendFsync, Config, Store};

const SYNC_EVERY: u64 = 500;
const MAINTENANCE_EVERY: u64 = 5_000;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: crash_writer <dir> [flags]");
    let mut shards = 1usize;
    let mut fsync = AppendFsync::EverySec;
    let (mut feed, mut rewrite, mut snapshot) = (false, false, false);
    let mut acked = false;
    let mut path: Option<(u64, bool)> = None; // (ring bytes, mapped)
    while let Some(a) = args.next() {
        match a.as_str() {
            "--shards" => shards = args.next().unwrap().parse().unwrap(),
            "--always" => fsync = AppendFsync::Always,
            "--feed" => feed = true,
            "--rewrite" => rewrite = true,
            "--snapshot" => snapshot = true,
            "--acked" => acked = true,
            "--ring" => path = Some((4 << 20, false)),
            "--mapped" => path = Some((4 << 20, true)),
            "--no-stage" => path = Some((0, false)),
            other => panic!("unknown flag {other}"),
        }
    }
    let mut cfg = Config::default().with_persist(&dir).with_shards(shards).with_appendfsync(fsync);
    if feed {
        cfg = cfg.with_feed(16 << 20);
    }
    if let Some((ring, mapped)) = path {
        cfg = cfg.with_stage_ring(ring).with_mapped_aof(mapped);
    }
    let store = Store::open(cfg).expect("open");

    let mut out = std::io::stdout().lock();
    // A monotone sequence key per shard-spread keyspace: `seq` carries the
    // loss-bound counter; k<i> keys give the AOF realistic bulk.
    let val = vec![0x62u8; 256];
    // Deliberately without end: this writer exists to be killed mid-write, so
    // `loop` says what a range to infinity only implied.
    let mut n = 0u64;
    loop {
        n += 1;
        store.set(format!("k{}", n % 1000).as_bytes(), &val).expect("set");
        store.set(b"seq", n.to_string().as_bytes()).expect("seq");
        if acked {
            writeln!(out, "ACKED {n}").unwrap();
            out.flush().unwrap();
        }
        if n.is_multiple_of(SYNC_EVERY) {
            store.fsync_aof().expect("fsync");
            writeln!(out, "SYNCED {n}").unwrap();
            out.flush().unwrap();
        }
        if n.is_multiple_of(MAINTENANCE_EVERY) {
            if rewrite {
                store.rewrite_aof().expect("rewrite");
            }
            if snapshot {
                store.save_snapshot().expect("snapshot");
            }
        }
    }
}
