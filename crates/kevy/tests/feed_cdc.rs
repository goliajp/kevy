//! FEED.* — end-to-end against a real 8-shard reactor with
//! `feed_enabled`. Covers the cursor contract:
//! tail/read round-trip, prefix filter (fail-open on multi-key),
//! FLUSHALL generation bump, clean-restart continuity, and
//! unclean-restart bump.

use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

const NSHARDS: usize = 8;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

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

struct Feed {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Feed {
    fn boot(dir: std::path::PathBuf) -> Self {
        let port = free_port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(NSHARDS)).bind([127, 0, 0, 1], port).shards(NSHARDS)
                .with_data_dir(dir_thread)
                .with_feed(true, 0);
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

    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "kevy-feed-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self::boot(dir)
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        s
    }

    fn shutdown(mut self) -> std::path::PathBuf {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let dir = self.dir.clone();
        std::mem::forget(self); // skip Drop's dir removal — caller reuses it
        dir
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Parse `*2 [:gen, :off]` from FEED.TAIL.
fn parse_tail(reply: &[u8]) -> (u64, u64) {
    let s = String::from_utf8_lossy(reply);
    let mut nums = s
        .lines()
        .filter(|l| l.starts_with(':'))
        .map(|l| l[1..].trim().parse::<u64>().unwrap());
    (nums.next().unwrap(), nums.next().unwrap())
}

#[test]
fn feed_shards_tail_read_roundtrip() {
    let srv = Feed::start();
    let mut c = srv.connect();
    assert_eq!(cmd(&mut c, &[b"FEED.SHARDS"]), format!(":{NSHARDS}\r\n").into_bytes());

    // Write keys until some shard has frames; find one via FEED.TAIL.
    for i in 0..32 {
        cmd(&mut c, &[b"SET", format!("fk{i}").as_bytes(), b"v"]);
    }
    let mut hit = None;
    for sh in 0..NSHARDS {
        let (g, off) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", sh.to_string().as_bytes()]));
        assert_eq!(g, 1, "fresh dir starts at generation 1");
        if off > 0 {
            hit = Some((sh, off));
            break;
        }
    }
    let (sh, off) = hit.expect("some shard saw writes");

    // Read from 0: frames come back in order with argv payloads.
    let r = cmd(
        &mut c,
        &[b"FEED.READ", sh.to_string().as_bytes(), b"1", b"0", b"COUNT", b"100"],
    );
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*3\r\n:1\r\n"), "got {s}");
    assert!(s.contains("SET"), "frames carry the effect argv: {s}");
    // Cursor advanced to the tail we saw.
    assert!(s.contains(&format!(":{off}\r\n")), "next cursor = tail: {s}");

    // Caught-up read = empty frame list, cursor unchanged.
    let r2 = cmd(
        &mut c,
        &[b"FEED.READ", sh.to_string().as_bytes(), b"1", off.to_string().as_bytes()],
    );
    let s2 = String::from_utf8_lossy(&r2);
    assert!(s2.ends_with("*0\r\n"), "caught up: {s2}");
}

#[test]
fn feed_prefix_filter_and_errors() {
    let srv = Feed::start();
    let mut c = srv.connect();
    // Two prefixes; both land wherever their keys hash.
    for i in 0..16 {
        cmd(&mut c, &[b"SET", format!("user:{i}").as_bytes(), b"u"]);
        cmd(&mut c, &[b"SET", format!("sess:{i}").as_bytes(), b"s"]);
    }
    // Find a shard with frames, read with PREFIX user:
    for sh in 0..NSHARDS {
        let (_, off) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", sh.to_string().as_bytes()]));
        if off == 0 {
            continue;
        }
        let r = cmd(
            &mut c,
            &[
                b"FEED.READ", sh.to_string().as_bytes(), b"1", b"0",
                b"COUNT", b"100", b"PREFIX", b"user:",
            ],
        );
        let s = String::from_utf8_lossy(&r);
        assert!(!s.contains("sess:"), "filtered out other prefixes: {s}");
    }
    // errors
    let r = cmd(&mut c, &[b"FEED.READ", b"99", b"1", b"0"]);
    assert!(r.starts_with(b"-ERR shard out of range"), "{:?}", String::from_utf8_lossy(&r));
    let r = cmd(&mut c, &[b"FEED.READ", b"0", b"9", b"0"]);
    assert!(
        r.starts_with(b"-ERR feed cursor ahead"),
        "{:?}",
        String::from_utf8_lossy(&r)
    );
}

#[test]
fn flushall_bumps_generation_and_old_cursor_resyncs() {
    let srv = Feed::start();
    let mut c = srv.connect();
    for i in 0..16 {
        cmd(&mut c, &[b"SET", format!("gb{i}").as_bytes(), b"v"]);
    }
    cmd(&mut c, &[b"FLUSHALL"]);
    // Post-flush: generation 2 everywhere, offsets restarted.
    let (g, off) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", b"0"]));
    assert_eq!(g, 2);
    assert_eq!(off, 0);
    // The old generation-1 cursor answers FEEDRESYNC with the new tail.
    let r = cmd(&mut c, &[b"FEED.READ", b"0", b"1", b"5"]);
    assert!(r.starts_with(b"-FEEDRESYNC 2 0"), "{:?}", String::from_utf8_lossy(&r));
}

#[test]
fn clean_restart_keeps_cursor_unclean_bumps() {
    let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "kevy-feed-restart-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // Gen 1: write, note a busy shard's tail, clean shutdown.
    let srv = Feed::boot(dir.clone());
    let mut c = srv.connect();
    for i in 0..32 {
        cmd(&mut c, &[b"SET", format!("rk{i}").as_bytes(), b"v"]);
    }
    let mut busy = (0usize, 0u64);
    for sh in 0..NSHARDS {
        let (_, off) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", sh.to_string().as_bytes()]));
        if off > busy.1 {
            busy = (sh, off);
        }
    }
    drop(c);
    let dir = srv.shutdown();

    // Clean restart: same generation, offset continues where it left.
    let srv2 = Feed::boot(dir.clone());
    let mut c2 = srv2.connect();
    let (g, off) = parse_tail(&cmd(&mut c2, &[b"FEED.TAIL", busy.0.to_string().as_bytes()]));
    assert_eq!(g, 1, "clean restart keeps the generation");
    assert_eq!(off, busy.1, "clean restart keeps the offset");
    drop(c2);
    let dir = srv2.shutdown(); // clean stop re-writes the markers…

    // …simulate a CRASH by deleting them: the next boot must bump.
    for i in 0..NSHARDS {
        std::fs::remove_file(dir.join(format!("feed-{i}.meta"))).unwrap();
    }
    let srv3 = Feed::boot(dir.clone());
    let mut c3 = srv3.connect();
    let (g3, off3) = parse_tail(&cmd(&mut c3, &[b"FEED.TAIL", busy.0.to_string().as_bytes()]));
    assert_eq!(g3, 2, "unclean restart bumps the generation");
    assert_eq!(off3, 0, "offsets restart after a bump");
    drop(c3);
    drop(srv3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_snapshot_records_feed_cursor() {
    let srv = Feed::start();
    let mut c = srv.connect();
    for i in 0..16 {
        cmd(&mut c, &[b"SET", format!("sc{i}").as_bytes(), b"v"]);
    }
    let r = cmd(&mut c, &[b"SAVE"]);
    assert!(r.starts_with(b"+OK"), "{:?}", String::from_utf8_lossy(&r));
    // SAVE returns +OK and the persist job commits behind it, so the dumps
    // appear shortly after. Poll for them rather than sleeping a fixed
    // beat: a fixed wait passes on an idle box and flakes on a loaded
    // shared runner, which is exactly how this test failed in CI while
    // passing 20/20 locally and 10/10 under 16-way CPU contention.
    // What is asserted is unchanged — every dump that exists must carry a
    // cursor equal to that shard's live tail, and at least one shard must
    // have had frames.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut seen_any = false;
    while !seen_any {
        for sh in 0..NSHARDS {
            let dump = srv.dir.join(format!("dump-{sh}.rdb"));
            if !dump.exists() {
                continue;
            }
            let cur = kevy_persist::read_snapshot_cursor(&dump).unwrap();
            let (g, off) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", sh.to_string().as_bytes()]));
            assert_eq!(cur, Some((g, off)), "shard {sh} snapshot cursor = live tail");
            if off > 0 {
                seen_any = true;
            }
        }
        assert!(
            seen_any || std::time::Instant::now() < deadline,
            "no shard had frames + a cursor within 20s of SAVE returning +OK",
        );
        if !seen_any {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

#[test]
fn prefix_stats_fanout() {
    let srv = Feed::start();
    let mut c = srv.connect();
    for i in 0..20 {
        cmd(&mut c, &[b"SET", format!("ps:{i}").as_bytes(), b"v"]);
    }
    cmd(&mut c, &[b"SET", b"other:1", b"v"]);
    cmd(&mut c, &[b"EXPIRE", b"ps:1", b"1000"]);
    let r = cmd(&mut c, &[b"PREFIX.STATS", b"ps:"]);
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("keys\r\n:20\r\n"), "20 keys under ps:, got {s}");
    assert!(s.contains("expires\r\n:1\r\n"), "1 ttl'd, got {s}");
}
