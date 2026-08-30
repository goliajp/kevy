//! crashgate's inspector process: reopen the store a crash_writer was
//! SIGKILLed under and report what survived, machine-readably:
//!
//!   RECOVERED <n>        the sequence counter that replay restored (0 if none)
//!   QUARANTINE <count>   corrupt-quarantine files present in the dir
//!   AOFBYTES <sum>       total AOF bytes across shards after open
//!
//! With --mark, additionally append 100 marked writes, fsync, and close
//! cleanly (`MARKED <n+100>`): the gate's no-blackhole probe — a SECOND
//! crash_check run must then recover at least n+100, or restarts are
//! rolling back (the 3.18 black hole).
//!
//!   crash_check <dir> [--shards N] [--feed] [--mark]
use kevy_embedded::{Config, Store};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: crash_check <dir> [flags]");
    let mut shards = 1usize;
    let (mut feed, mut mark, mut resync) = (false, false, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--shards" => shards = args.next().unwrap().parse().unwrap(),
            "--feed" => feed = true,
            "--mark" => mark = true,
            "--resync" => resync = true,
            other => panic!("unknown flag {other}"),
        }
    }
    let mut cfg =
        Config::default().with_persist(&dir).with_shards(shards).with_replay_resync(resync);
    if feed {
        cfg = cfg.with_feed(16 << 20);
    }
    let store = Store::open(cfg).expect("open after crash must succeed");

    let recovered: u64 = store
        .get(b"seq")
        .expect("get seq")
        .map(|v| String::from_utf8_lossy(&v).parse().unwrap_or(0))
        .unwrap_or(0);
    println!("RECOVERED {recovered}");

    let mut quarantine = 0u32;
    let mut aof_bytes = 0u64;
    for entry in std::fs::read_dir(&dir).expect("read dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains("quarantine") {
            quarantine += 1;
        }
        if name.starts_with("aof-") && name.ends_with(".aof") {
            aof_bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    println!("QUARANTINE {quarantine}");
    println!("AOFBYTES {aof_bytes}");

    // Payload-integrity scan: crash_writer only ever writes 0x62-uniform
    // 256-byte values into k<i>, so any surviving k-key whose bytes differ
    // was corrupted on disk and replayed anyway — the silent-taint case a
    // frame CRC (T5) exists to catch.
    let mut tainted = 0u32;
    for i in 0..1000u32 {
        if let Ok(Some(v)) = store.get(format!("k{i}").as_bytes())
            && !(v.len() == 256 && v.iter().all(|&b| b == 0x62))
        {
            tainted += 1;
        }
    }
    println!("TAINTED {tainted}");

    if mark {
        let val = vec![0x6du8; 64];
        for i in 1..=100u64 {
            store.set(format!("mark{i}").as_bytes(), &val).expect("mark");
            store.set(b"seq", (recovered + i).to_string().as_bytes()).expect("seq");
        }
        store.fsync_aof().expect("fsync");
        println!("MARKED {}", recovered + 100);
    }
}
