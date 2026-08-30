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

use kevy_testnet::free_port;

mod common;

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
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(NSHARDS))
                .bind([127, 0, 0, 1], port)
                .shards(NSHARDS)
                .with_data_dir(dir_thread)
                .with_feed(true, 0);
            rt.run(stop_thread).unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "kevy-feed-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
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
    let mut nums =
        s.lines().filter(|l| l.starts_with(':')).map(|l| l[1..].trim().parse::<u64>().unwrap());
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
        assert_ne!(g, 0, "fresh dir draws a random nonzero generation");
        if off > 0 {
            hit = Some((sh, g, off));
            break;
        }
    }
    let (sh, live_gen, off) = hit.expect("some shard saw writes");

    // Read from 0: frames come back in order with argv payloads.
    let r = cmd(
        &mut c,
        &[
            b"FEED.READ",
            sh.to_string().as_bytes(),
            live_gen.to_string().as_bytes(),
            b"0",
            b"COUNT",
            b"100",
        ],
    );
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with(&format!("*3\r\n:{live_gen}\r\n")), "got {s}");
    assert!(s.contains("SET"), "frames carry the effect argv: {s}");
    // Cursor advanced to the tail we saw.
    assert!(s.contains(&format!(":{off}\r\n")), "next cursor = tail: {s}");

    // Caught-up read = empty frame list, cursor unchanged.
    let r2 = cmd(
        &mut c,
        &[
            b"FEED.READ",
            sh.to_string().as_bytes(),
            live_gen.to_string().as_bytes(),
            off.to_string().as_bytes(),
        ],
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
        let (g, off) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", sh.to_string().as_bytes()]));
        if off == 0 {
            continue;
        }
        let r = cmd(
            &mut c,
            &[
                b"FEED.READ",
                sh.to_string().as_bytes(),
                g.to_string().as_bytes(),
                b"0",
                b"COUNT",
                b"100",
                b"PREFIX",
                b"user:",
            ],
        );
        let s = String::from_utf8_lossy(&r);
        assert!(!s.contains("sess:"), "filtered out other prefixes: {s}");
    }
    // errors
    let r = cmd(&mut c, &[b"FEED.READ", b"99", b"1", b"0"]);
    assert!(r.starts_with(b"-ERR shard out of range"), "{:?}", String::from_utf8_lossy(&r));
    // A generation the feed never issued (identities are random now,
    // so ANY foreign gen — a counter's "9" included) → resync, never
    // a quiet stream.
    let r = cmd(&mut c, &[b"FEED.READ", b"0", b"9", b"0"]);
    assert!(r.starts_with(b"-FEEDRESYNC "), "{:?}", String::from_utf8_lossy(&r));
    // An offset ahead of the stream in the LIVE generation → cursor-ahead.
    let (g0, _) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", b"0"]));
    let r = cmd(&mut c, &[b"FEED.READ", b"0", g0.to_string().as_bytes(), b"999999"]);
    assert!(r.starts_with(b"-ERR feed cursor ahead"), "{:?}", String::from_utf8_lossy(&r));
}

#[test]
fn flushall_bumps_generation_and_old_cursor_resyncs() {
    let srv = Feed::start();
    let mut c = srv.connect();
    for i in 0..16 {
        cmd(&mut c, &[b"SET", format!("gb{i}").as_bytes(), b"v"]);
    }
    let (g_before, _) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", b"0"]));
    cmd(&mut c, &[b"FLUSHALL"]);
    // Post-flush: a fresh generation everywhere, offsets restarted.
    let (g, off) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", b"0"]));
    assert_ne!(g, g_before);
    assert_eq!(off, 0);
    // The old-generation cursor answers FEEDRESYNC with the new tail.
    let r = cmd(&mut c, &[b"FEED.READ", b"0", g_before.to_string().as_bytes(), b"5"]);
    let want = format!("-FEEDRESYNC {g} 0");
    assert!(r.starts_with(want.as_bytes()), "want {want:?}, got {:?}", String::from_utf8_lossy(&r));
}

#[test]
fn clean_restart_keeps_cursor_unclean_bumps() {
    let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "kevy-feed-restart-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // Gen 1: write, note a busy shard's tail, clean shutdown.
    let srv = Feed::boot(dir.clone());
    let mut c = srv.connect();
    for i in 0..32 {
        cmd(&mut c, &[b"SET", format!("rk{i}").as_bytes(), b"v"]);
    }
    let mut busy = (0usize, 0u64);
    let mut busy_gen = 0u64;
    for sh in 0..NSHARDS {
        let (g, off) = parse_tail(&cmd(&mut c, &[b"FEED.TAIL", sh.to_string().as_bytes()]));
        if off > busy.1 {
            busy = (sh, off);
            busy_gen = g;
        }
    }
    drop(c);
    let dir = srv.shutdown();

    // Clean restart: same generation, offset continues where it left.
    let srv2 = Feed::boot(dir.clone());
    let mut c2 = srv2.connect();
    let (g, off) = parse_tail(&cmd(&mut c2, &[b"FEED.TAIL", busy.0.to_string().as_bytes()]));
    assert_eq!(g, busy_gen, "clean restart keeps the generation");
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
    assert_ne!(g3, busy_gen, "unclean restart draws a fresh generation");
    assert_ne!(g3, 0);
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

/// A cross-shard `RENAME` must reach the change feed on both ends.
///
/// It did not. Until 2026-08-05 the destination write (`Op::RenamePut`)
/// carried no logging at all — not AOF, not the feed — so a consumer
/// rebuilding state from frames kept the old key forever and never
/// learned the new one. Nobody hit it because nobody consumes the feed
/// yet; that is exactly the reason to hold it with a test rather than
/// with adoption.
#[test]
fn cross_shard_rename_reaches_the_feed_on_both_ends() {
    let cross = |a: &[u8], b: &[u8]| {
        kevy_rt::shard_of_key(a, NSHARDS, false) != kevy_rt::shard_of_key(b, NSHARDS, false)
    };
    assert!(cross(b"fsrc", b"fdst"), "fixture must straddle two shards");
    let src_sh = kevy_rt::shard_of_key(b"fsrc", NSHARDS, false);
    let dst_sh = kevy_rt::shard_of_key(b"fdst", NSHARDS, false);

    let srv = Feed::start();
    let mut c = srv.connect();
    cmd(&mut c, &[b"SET", b"fsrc", b"payload"]);
    assert_eq!(cmd(&mut c, &[b"RENAME", b"fsrc", b"fdst"]), b"+OK\r\n");

    let frames = |c: &mut std::net::TcpStream, sh: usize| {
        let (g, _) = parse_tail(&cmd(c, &[b"FEED.TAIL", sh.to_string().as_bytes()]));
        String::from_utf8_lossy(&cmd(
            c,
            &[
                b"FEED.READ",
                sh.to_string().as_bytes(),
                g.to_string().as_bytes(),
                b"0",
                b"COUNT",
                b"100",
            ],
        ))
        .into_owned()
    };

    // The destination shard learns the value, not just the name.
    let dst = frames(&mut c, dst_sh);
    assert!(dst.contains("fdst"), "destination shard saw no frame for fdst: {dst}");
    assert!(dst.contains("payload"), "the value must travel with it: {dst}");

    // The source shard records the removal — an idempotent consumer
    // replaying both streams must not end up holding the key twice.
    let src = frames(&mut c, src_sh);
    assert!(src.contains("DEL"), "source shard saw no removal of fsrc: {src}");
    assert!(src.contains("fsrc"), "the removal must name the key: {src}");
}

/// A cross-shard BITOP's result reaches the feed on the DESTINATION's
/// shard — which is the same question as "did it reach a replica",
/// because the feed reads the replication backlog.
///
/// `propgate` holds this at the source: every durable write in the
/// runtime must be paired with a push, or named. It caught BITOP
/// writing its result with `log_write` — the AOF alone — which is the
/// exact shape of the three data-loss bugs that gate was written after.
/// A source lint is the right place to catch it; this is the place to
/// see it.
#[test]
fn a_cross_shard_bitop_result_reaches_the_feed_on_the_destination_shard() {
    let srv = Feed::start();
    let mut c = srv.connect();

    // Sources and destination on three different shards, asked of the
    // function the server routes with rather than assumed.
    let of = |k: &str| kevy_rt::shard_of_key(k.as_bytes(), NSHARDS, false);
    let mut names: Vec<String> = Vec::new();
    for i in 0..4000 {
        let k = format!("bitfeed-{i}");
        if names.iter().all(|p| of(p) != of(&k)) {
            names.push(k);
        }
        if names.len() == 3 {
            break;
        }
    }
    assert_eq!(names.len(), 3, "no three names landed on three shards");
    let (src_a, src_b, dst) = (&names[0], &names[1], &names[2]);

    cmd(&mut c, &[b"SETBIT", src_a.as_bytes(), b"0", b"1"]);
    cmd(&mut c, &[b"SETBIT", src_b.as_bytes(), b"3", b"1"]);
    // Where the destination's shard stands before the BITOP.
    let dst_shard = of(dst);
    let (generation, before) =
        parse_tail(&cmd(&mut c, &[b"FEED.TAIL", dst_shard.to_string().as_bytes()]));

    let reply = cmd(&mut c, &[b"BITOP", b"OR", dst.as_bytes(), src_a.as_bytes(), src_b.as_bytes()]);
    assert_eq!(reply, b":1\r\n".to_vec(), "BITOP did not store one byte");

    let frames = cmd(
        &mut c,
        &[
            b"FEED.READ",
            dst_shard.to_string().as_bytes(),
            generation.to_string().as_bytes(),
            before.to_string().as_bytes(),
            b"COUNT",
            b"100",
        ],
    );
    let text = String::from_utf8_lossy(&frames);
    assert!(
        text.contains(dst.as_str()),
        "the destination's shard feed never saw {dst} after a cross-shard \
         BITOP — the result was durable and unpropagated: {text}"
    );
}
