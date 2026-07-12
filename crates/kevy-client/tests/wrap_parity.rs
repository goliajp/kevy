//! Wrap-parity — every wrap exercised against a real kevy server:
//! an in-process 8-shard kevy-rt reactor running the full kevy command
//! set (the same harness as crates/kevy/tests/*), with the change feed
//! and a persistent data dir enabled so FEED.* and IDX.* are live.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kevy_client::{Connection, HExpireCond, IdxType, Reply, ZAggregate};

static START_GATE: Mutex<()> = Mutex::new(());

const NSHARDS: usize = 8;

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-client-parity-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
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
            std::thread::sleep(Duration::from_millis(5));
        }
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn conn(&self) -> Connection {
        Connection::connect(&format!("kevy://127.0.0.1:{}", self.port)).unwrap()
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

// ───────────────────────── blocking pops ─────────────────────────

#[test]
fn blocking_pop_immediate_when_data_present() {
    let srv = Server::start();
    let mut c = srv.conn();
    c.rpush(b"bq", &[&b"first"[..], &b"second"[..]]).unwrap();

    let hit = c.blpop(&[&b"bq"[..]], Some(Duration::from_secs(2))).unwrap();
    assert_eq!(hit, Some((b"bq".to_vec(), b"first".to_vec())));
    let hit = c.brpop(&[&b"bq"[..]], Some(Duration::from_secs(2))).unwrap();
    assert_eq!(hit, Some((b"bq".to_vec(), b"second".to_vec())));

    c.zadd(b"bz", &[(3.0, b"hi".as_ref()), (1.5, b"lo".as_ref())])
        .unwrap();
    let hit = c
        .bzpopmin(&[&b"bz"[..]], Some(Duration::from_secs(2)))
        .unwrap();
    assert_eq!(hit, Some((b"bz".to_vec(), b"lo".to_vec(), 1.5)));
}

#[test]
fn blocking_pop_timeout_returns_none() {
    let srv = Server::start();
    let mut c = srv.conn();
    let t0 = std::time::Instant::now();
    let hit = c
        .blpop(&[&b"never"[..]], Some(Duration::from_millis(300)))
        .unwrap();
    assert_eq!(hit, None);
    assert!(t0.elapsed() >= Duration::from_millis(250), "parked for real");
    let hit = c
        .bzpopmin(&[&b"never-z"[..]], Some(Duration::from_millis(300)))
        .unwrap();
    assert_eq!(hit, None);
}

#[test]
fn blocking_pop_wakes_on_push_from_other_connection() {
    let srv = Server::start();
    let mut waiter = srv.conn();
    let mut pusher = srv.conn();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        pusher.rpush(b"wake-q", &[&b"payload"[..]]).unwrap();
    });
    let hit = waiter
        .blpop(&[&b"wake-q"[..]], Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(hit, Some((b"wake-q".to_vec(), b"payload".to_vec())));
    t.join().unwrap();
}

// ───────────────────────── hash field-TTL ─────────────────────────

#[test]
fn hash_field_ttl_round_trip() {
    let srv = Server::start();
    let mut c = srv.conn();
    c.hset(b"h", &[(b"a".as_ref(), b"1".as_ref()), (b"b".as_ref(), b"2".as_ref())])
        .unwrap();

    let codes = c
        .hexpire(b"h", &[&b"a"[..], &b"nope"[..]], Duration::from_secs(120), HExpireCond::Always)
        .unwrap();
    assert_eq!(codes, vec![1, -2]);

    // HTTL is SECONDS. The old assertion here was `(1..=120_000)`, which a
    // millisecond reply satisfies just as happily as a second one — and that is
    // precisely how HTTL shipped returning milliseconds. Bound it tight enough
    // that only the right unit fits.
    let ttls = c.httl(b"h", &[&b"a"[..], &b"b"[..]]).unwrap();
    assert!((110..=120).contains(&ttls[0]), "HTTL must reply seconds: {}", ttls[0]);
    assert_eq!(ttls[1], -1);

    // HPTTL is MILLISECONDS, over the very same deadline.
    let ms = c.hpttl(b"h", &[&b"a"[..]]).unwrap()[0];
    assert!((110_000..=120_000).contains(&ms), "HPTTL must reply millis: {ms}");

    // NX refuses to overwrite the existing deadline.
    let codes = c
        .hexpire(b"h", &[&b"a"[..]], Duration::from_secs(300), HExpireCond::Nx)
        .unwrap();
    assert_eq!(codes, vec![0]);

    let codes = c
        .hpexpire(b"h", &[&b"b"[..]], Duration::from_millis(90_500), HExpireCond::Always)
        .unwrap();
    assert_eq!(codes, vec![1]);
    // 90_500 ms rounds to 91 s — HTTL rounds to nearest, as the key-level TTL
    // and Redis both do.
    let ttl = c.httl(b"h", &[&b"b"[..]]).unwrap()[0];
    assert!((85..=91).contains(&ttl), "HTTL must reply seconds: {ttl}");
    let ms = c.hpttl(b"h", &[&b"b"[..]]).unwrap()[0];
    assert!((85_000..=90_500).contains(&ms), "HPTTL must reply millis: {ms}");

    let codes = c.hpersist(b"h", &[&b"a"[..], &b"b"[..]]).unwrap();
    assert_eq!(codes, vec![1, 1]);
    assert_eq!(c.httl(b"h", &[&b"a"[..], &b"b"[..]]).unwrap(), vec![-1, -1]);
    // the sentinels are units-free and must pass through both verbs untouched
    assert_eq!(c.hpttl(b"h", &[&b"a"[..], &b"gone"[..]]).unwrap(), vec![-1, -2]);
}

// ───────────────────────── zset algebra ─────────────────────────

#[test]
fn zset_algebra_store_and_card() {
    let srv = Server::start();
    let mut c = srv.conn();
    c.zadd(b"za", &[(1.0, b"x".as_ref()), (2.0, b"y".as_ref())])
        .unwrap();
    c.zadd(b"zb", &[(10.0, b"y".as_ref()), (20.0, b"z".as_ref())])
        .unwrap();

    assert_eq!(c.zinterstore(b"zi", &[&b"za"[..], &b"zb"[..]]).unwrap(), 1);
    assert_eq!(c.zscore(b"zi", b"y").unwrap(), Some(12.0)); // SUM

    assert_eq!(c.zunionstore(b"zu", &[&b"za"[..], &b"zb"[..]]).unwrap(), 3);

    let n = c
        .zunionstore_with(
            b"zw",
            &[&b"za"[..], &b"zb"[..]],
            Some(&[2.0, 1.0]),
            ZAggregate::Max,
        )
        .unwrap();
    assert_eq!(n, 3);
    assert_eq!(c.zscore(b"zw", b"y").unwrap(), Some(10.0)); // max(2*2, 10*1)

    assert_eq!(c.zintercard(&[&b"za"[..], &b"zb"[..]], None).unwrap(), 1);
    assert_eq!(c.zintercard(&[&b"za"[..], &b"zb"[..]], Some(1)).unwrap(), 1);
}

// ───────────────────────── IDX.* ─────────────────────────

/// Poll a closure until the index build finishes (ticks run ~10 Hz;
/// queries answer `INDEXBUILDING …` while backfilling).
fn idx_ready<T>(mut f: impl FnMut() -> kevy_client::KevyResult<T>) -> T {
    for _ in 0..100 {
        match f() {
            Ok(v) => return v,
            Err(e) if e.to_string().contains("INDEXBUILDING") => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("index query failed: {e}"),
        }
    }
    panic!("index never became ready");
}

#[test]
fn idx_create_query_list_drop_round_trip() {
    let srv = Server::start();
    let mut c = srv.conn();
    for i in 0..20 {
        c.hset(
            format!("user:{i}").as_bytes(),
            &[(b"age".as_ref(), format!("{}", 20 + i).into_bytes().as_slice())],
        )
        .unwrap();
    }

    c.idx_create_range(b"age_idx", b"user:", b"age", IdxType::I64)
        .unwrap();

    // RANGE with pagination: 5 + rest, cursor round-trips, no overlap.
    let p1 = idx_ready(|| c.idx_query_range(b"age_idx", b"20", b"31", 5, None));
    assert_eq!(p1.rows.len(), 5);
    assert_eq!(p1.rows[0].key, b"user:0");
    assert_eq!(p1.rows[0].value, b"20");
    let cursor = p1.cursor.expect("more pages");
    let p2 = c
        .idx_query_range(b"age_idx", b"20", b"31", 100, Some(&cursor))
        .unwrap();
    assert_eq!(p2.rows.len(), 7, "12 hits total = 5 + 7");
    assert_eq!(p2.cursor, None);
    for row in &p1.rows {
        assert!(p2.rows.iter().all(|r| r.key != row.key), "no overlap");
    }

    // EQ.
    let eq = c.idx_query_eq(b"age_idx", b"25", 10).unwrap();
    assert_eq!(eq.rows.len(), 1);
    assert_eq!(eq.rows[0].key, b"user:5");

    // Raw escape hatch: same query through the argv passthrough.
    let raw = c
        .idx_query_raw(&[b"age_idx", b"EQ", b"25", b"LIMIT", b"10"])
        .unwrap();
    assert!(matches!(raw, Reply::Array(_)));

    // LIST reports the index as ready with 20 entries.
    let infos = c.idx_list().unwrap();
    let info = infos.iter().find(|i| i.name == b"age_idx").expect("listed");
    assert_eq!(info.prefix, b"user:");
    assert_eq!(info.kind, "range");
    assert_eq!(info.state, "ready");
    assert_eq!(info.entries, 20);

    // Raw create: a second (str) index via the passthrough.
    c.idx_create_raw(&[
        b"name_idx", b"ON", b"PREFIX", b"user:", b"FIELD", b"age", b"TYPE", b"str", b"KIND",
        b"range",
    ])
    .unwrap();
    assert_eq!(c.idx_list().unwrap().len(), 2);

    // DROP: true then false.
    assert!(c.idx_drop(b"name_idx").unwrap());
    assert!(!c.idx_drop(b"name_idx").unwrap());
    assert!(c.idx_drop(b"age_idx").unwrap());
    let err = c.idx_query_eq(b"age_idx", b"25", 1).unwrap_err();
    assert!(err.to_string().contains("no such index"), "err = {err}");
}

// ───────────────────────── FEED.* ─────────────────────────

#[test]
fn feed_shards_tail_read_round_trip() {
    let srv = Server::start();
    let mut c = srv.conn();
    assert_eq!(c.feed_shards().unwrap(), NSHARDS);

    for i in 0..32 {
        c.set(format!("fk{i}").as_bytes(), b"v").unwrap();
    }

    // Find a shard that saw writes; a fresh dir starts at generation 1.
    let mut hit = None;
    for sh in 0..NSHARDS {
        let (generation, off) = c.feed_tail(sh).unwrap();
        assert_eq!(generation, 1, "fresh dir starts at generation 1");
        if off > 0 {
            hit = Some((sh, off));
            break;
        }
    }
    let (sh, tail_off) = hit.expect("some shard saw writes");

    let batch = c.feed_read(sh, 1, 0, None, &[]).unwrap();
    assert_eq!(batch.generation, 1);
    assert_eq!(batch.next_offset, tail_off);
    assert!(!batch.frames.is_empty());
    let frame = &batch.frames[0];
    assert!(frame.argv[0].eq_ignore_ascii_case(b"SET"), "argv = {:?}", frame.argv);
    assert!(frame.argv[1].starts_with(b"fk"));

    // COUNT clamps the page; the cursor still advances monotonically.
    let page = c.feed_read(sh, 1, 0, Some(1), &[]).unwrap();
    assert_eq!(page.frames.len(), 1);
    assert!(page.next_offset <= tail_off);

    // A prefix that matches nothing filters every frame out (SET's
    // key layout is cheap to determine → not fail-open).
    let none = c.feed_read(sh, 1, 0, None, &[b"other:"]).unwrap();
    assert!(none.frames.is_empty());
    assert_eq!(none.next_offset, tail_off, "filtering never moves the cursor differently");
}

// ───────────────────────── pipeline ─────────────────────────

#[test]
fn pipeline_sends_batch_and_reads_replies_in_order() {
    let srv = Server::start();
    let mut c = srv.conn();
    let replies = c
        .pipeline(|p| {
            p.cmd(&[b"SET", b"pk", b"pv"])
                .cmd(&[b"INCR", b"pn"])
                .cmd(&[b"GET", b"pk"])
                .cmd(&[b"GET", b"missing"]);
        })
        .unwrap();
    assert_eq!(replies.len(), 4);
    assert!(matches!(&replies[0], Reply::Simple(s) if s == b"OK"));
    assert!(matches!(&replies[1], Reply::Int(1)));
    assert!(matches!(&replies[2], Reply::Bulk(v) if v == b"pv"));
    assert!(matches!(&replies[3], Reply::Nil));

    // Per-command errors come back in-slot, not as io::Error.
    let replies = c
        .pipeline(|p| {
            p.cmd(&[b"INCR", b"pk"]).cmd(&[b"GET", b"pk"]);
        })
        .unwrap();
    assert!(matches!(&replies[0], Reply::Error(_)));
    assert!(matches!(&replies[1], Reply::Bulk(v) if v == b"pv"));

    // Empty batch: no wire round-trip, empty reply set.
    assert!(c.pipeline(|_| {}).unwrap().is_empty());
}
