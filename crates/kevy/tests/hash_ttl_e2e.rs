//! Hash field TTLs — real-server e2e: the relative `HEXPIRE`
//! must survive an AOF replay WITHOUT re-anchoring (the
//! `HPEXPIREAT` follow-up frame `Shard::log_write` appends), and the
//! reaper must sweep due fields.

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

/// Send one command and read back exactly one COMPLETE reply.
///
/// This was "sleep 30 ms, then `read()` once", which holds while every
/// reply arrives in one segment and desynchronises the connection the
/// first time one does not: the tail stays in the socket and every later
/// reply is read a frame late. `table_e2e`'s copy failed exactly that way
/// under load — `EXEC` answers `*1\r\n:0\r\n`, an assertion checked only
/// the `*1\r\n` prefix, and `:0\r\n` came back on the front of the next
/// reply. The frame now says when it is complete, and anything left over
/// is a failure rather than a shift.
fn cmd(s: &mut std::net::TcpStream, parts: &[&[u8]]) -> Vec<u8> {
    s.write_all(&req(parts)).unwrap();
    let mut buf = Vec::new();
    loop {
        if let Some(n) = common::reply_len(&buf) {
            assert_eq!(
                n,
                buf.len(),
                "{} extra byte(s) after the reply — the connection is a frame ahead: {:?}",
                buf.len() - n,
                String::from_utf8_lossy(&buf[n..]).chars().take(60).collect::<String>()
            );
            return buf;
        }
        let mut chunk = [0u8; 65536];
        let got = s.read(&mut chunk).unwrap();
        assert!(got > 0, "server closed mid-reply (have {} bytes)", buf.len());
        buf.extend_from_slice(&chunk[..got]);
    }
}

use kevy_testnet::free_port;

mod common;

fn boot(port: u16, dir: std::path::PathBuf, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(4))
            .bind([127, 0, 0, 1], port)
            .shards(4)
            .with_data_dir(dir)
            .with_aof(true);
        rt.run(stop).unwrap();
    })
}

fn wait_up(port: u16) {
    for _ in 0..400 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("server did not come up");
}

#[test]
fn hexpire_survives_replay_without_reanchor() {
    let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "kevy-hfttl-e2e-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let port1 = free_port();
    let stop1 = Arc::new(AtomicBool::new(false));
    let h1 = boot(port1, dir.clone(), stop1.clone());
    wait_up(port1);
    let observed_ms: i64;
    {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port1)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        cmd(&mut c, &[b"HSET", b"eh", b"f", b"v", b"plain", b"p"]);
        // relative form: 100s
        let r = cmd(&mut c, &[b"HEXPIRE", b"eh", b"100", b"FIELDS", b"1", b"f"]);
        assert_eq!(r, b"*1\r\n:1\r\n");
        // HPTTL for the millisecond precision this test needs. HTTL is the
        // SECONDS verb — asserting ~100_000 out of it is what let the unit bug
        // hide here in the first place.
        let r = cmd(&mut c, &[b"HPTTL", b"eh", b"FIELDS", b"1", b"f"]);
        let s = String::from_utf8_lossy(&r);
        observed_ms = s.trim_start_matches("*1\r\n:").trim_end().parse().unwrap();
        assert!(observed_ms > 90_000 && observed_ms <= 100_000, "{observed_ms}");
        // and the same deadline, read in seconds, is ~100 — not ~100000.
        let r = cmd(&mut c, &[b"HTTL", b"eh", b"FIELDS", b"1", b"f"]);
        let s = String::from_utf8_lossy(&r);
        let secs: i64 = s.trim_start_matches("*1\r\n:").trim_end().parse().unwrap();
        assert!((90..=100).contains(&secs), "HTTL must reply seconds, got {secs}");
    }
    // sleep a beat so a re-anchored replay would visibly RESET to ~100s
    std::thread::sleep(std::time::Duration::from_millis(1200));
    stop1.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port1));
    h1.join().unwrap();

    let port2 = free_port();
    let stop2 = Arc::new(AtomicBool::new(false));
    let h2 = boot(port2, dir.clone(), stop2.clone());
    wait_up(port2);
    {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port2)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        // HPTTL again: this compares against observed_ms, so it must be the
        // same unit. Reading it in seconds would make `f_ttl < observed_ms
        // - 1000` trivially true and quietly retire the assertion.
        let r = cmd(&mut c, &[b"HPTTL", b"eh", b"FIELDS", b"2", b"f", b"plain"]);
        let s = String::from_utf8_lossy(&r);
        let mut nums =
            s.lines().filter(|l| l.starts_with(':')).map(|l| l[1..].parse::<i64>().unwrap());
        let f_ttl = nums.next().unwrap();
        let plain = nums.next().unwrap();
        // wall clock advanced ≥1.2s across restart: a correctly
        // anchored deadline reads BELOW the pre-restart observation.
        assert!(
            f_ttl > 0 && f_ttl < observed_ms - 1000,
            "no re-anchor: pre {observed_ms}ms post {f_ttl}ms"
        );
        assert_eq!(plain, -1);
        // due field sweeps: set a past deadline and read it gone
        let r = cmd(&mut c, &[b"HPEXPIREAT", b"eh", b"10", b"FIELDS", b"1", b"f"]);
        assert_eq!(r, b"*1\r\n:2\r\n");
        let r = cmd(&mut c, &[b"HEXISTS", b"eh", b"f"]);
        assert_eq!(r, b":0\r\n");
    }
    stop2.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port2));
    h2.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The tick's sweep reaps a due field, and nothing had to read it.
///
/// `sweep_hash_field_ttls` is the only path that calls `Store::tick_hash_ttl`,
/// and it exists so a field expiring reaches the write hook — a covering
/// `VALUES` copy must not outlive the field it copies. Every hash operation
/// purges lazily on access, so a test that reads the field proves nothing
/// about the sweep: the read would have removed it either way.
///
/// So the deadline is installed through the snapshot loader hook, which does
/// not take the command path's immediate-delete branch, and the accounting is
/// read with `hash_ttl_each`, which iterates without purging. If the tick did
/// not sweep, the entry is still there.
///
/// It is also the reason this is not written with a sleep. The sweep only has
/// work when a deadline falls due between ticks, which under a loaded runner
/// may or may not happen inside any particular window — and a symbol that is
/// covered on some runs and not others is a ratchet that fires on the weather.
#[test]
fn the_tick_sweeps_a_due_hash_field_without_anyone_reading_it() {
    use kevy_rt::Commands as _;

    let kevy = kevy::KevyCommands::new();
    let mut store = kevy::KeyspaceStore::new();

    let argv =
        |parts: &[&[u8]]| kevy::Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
    let r = kevy.dispatch(&mut store, &argv(&[b"HSET", b"h", b"f", b"v"]));
    assert_eq!(r, b":1\r\n");

    // Past deadline, installed the way a snapshot restores one — the command
    // path would delete the field itself and never reach the sweep.
    store.load_hash_field_ttl(b"h", b"f", 1);
    let mut before = 0usize;
    store.hash_ttl_each(|_, _, _| before += 1);
    assert_eq!(before, 1, "the deadline is installed");

    kevy.on_shard_tick(&mut store);

    let mut after = 0usize;
    store.hash_ttl_each(|_, _, _| after += 1);
    assert_eq!(after, 0, "the sweep reaped it; no read was involved");
    assert_eq!(kevy.dispatch(&mut store, &argv(&[b"HEXISTS", b"h", b"f"])), b":0\r\n");
}
