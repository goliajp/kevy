//! T6 e2e — hydration cold-batching + no-promote peek adoption against
//! a real 8-shard tiered reactor, with an UNTIERED twin server fed the
//! same ops (the transparency principle: replies must be
//! byte-identical).
//!
//! One scripted flow covers the three bulk paths:
//! - PREFIX.DIGEST over a mostly-cold prefix == the hot twin's digest,
//!   zero promotions (peek_scope adoption);
//! - IDX.CREATE **after** the rows went cold → backfill completes over
//!   the peek (zero promotions), IDX.VERIFY drift-free on both;
//! - IDX.QUERY … FIELDS on the cold table → reply byte-identical to
//!   the hot twin, `peek_preads_total` == cold rows hydrated (one read
//!   per ROW, not per field), `batch_submissions_total` ≤ shard count
//!   (one batch per shard page), promotions_total still 0.
//!
//! Demotion here is driven by the budget watermark (a small
//! `with_tier_budget`), made deterministic by POLLING `cold_keys` to a
//! stable fixpoint before any assertion — never by sleeping on
//! eviction timing.

use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

mod common;

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

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// `tier_budget = None` → the untiered hot twin.
    fn start(tier_budget: Option<u64>) -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-tierhyd-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(8))
                .bind([127, 0, 0, 1], port)
                .shards(8)
                .with_data_dir(dir_thread)
                .with_tier_budget(tier_budget);
            rt.run(stop_thread).unwrap();
        });
        kevy_testnet::assert_listening(port, "the server under test");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn connect(&self) -> std::net::TcpStream {
        let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        s
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

/// Pull one integer gauge out of an INFO reply.
fn info_gauge(s: &mut std::net::TcpStream, name: &str) -> u64 {
    info_gauges(s, &[name])[0]
}

/// Every named gauge out of ONE `INFO` reply.
///
/// Gauges compared to each other must come from one aggregation pass:
/// `INFO` refreshes the answering shard and then sums every shard's
/// published slot, so two round trips are two different moments, and an
/// equality asserted across them is asserting something the engine never
/// promised.
fn info_gauges(s: &mut std::net::TcpStream, names: &[&str]) -> Vec<u64> {
    let reply = String::from_utf8_lossy(&cmd(s, &[b"INFO"])).into_owned();
    names
        .iter()
        .map(|name| {
            reply
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{name}:")))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or_else(|| panic!("INFO has no {name} gauge:\n{reply}"))
        })
        .collect()
}

/// Poll `cold_keys` until it is nonzero AND stable across two
/// consecutive reads — the deterministic "demotion has drained"
/// fixpoint (never a bare sleep on eviction timing).
fn wait_cold_stable(s: &mut std::net::TcpStream) -> u64 {
    // Two equal reads 150 ms apart is not settled: seven of eight shards
    // publish `cold_keys` only on their own tick, so a value can hold
    // still across two reads and then rise when a late shard reports.
    // That is how this returned 50 where 52 rows were cold, and the
    // "rows stay cold" assertion later compared a fresh read against it.
    // Nonzero FIRST — zero is perfectly stable before demotion starts, so
    // resting alone would settle on it. Then rest, because two equal reads
    // 150 ms apart is not settled: seven of eight shards publish
    // `cold_keys` only on their own tick, and a value can hold still
    // across two reads and rise when a late shard reports.
    common::until("cold_keys to become nonzero", || info_gauge(s, "cold_keys") > 0);
    common::at_rest("cold_keys", || info_gauge(s, "cold_keys"))
}

fn write_rows(s: &mut std::net::TcpStream) {
    // 60 hash rows × ~8 KB pad: on the tiered server's 256 KB budget
    // (32 KB/shard, watermark ≈ 30 KB) most rows must spill, while the
    // stub + segment floor stays far under the watermark (so
    // IDX.CREATE is not floor-refused). Only `a`/`n` are hydrated —
    // the pad exists to create spill pressure, not reply bytes.
    let pad = vec![b'p'; 8192];
    for i in 0..60u32 {
        let key = format!("row:{i:02}");
        let n = i.to_string();
        let a = format!("alpha-{i}");
        let reply = cmd(
            s,
            &[b"HSET", key.as_bytes(), b"n", n.as_bytes(), b"a", a.as_bytes(), b"pad", &pad],
        );
        assert_eq!(reply, b":3\r\n", "HSET {key}");
    }
}

const QUERY: &[&[u8]] = &[
    b"IDX.QUERY", b"byn", b"RANGE", b"0", b"100", b"LIMIT", b"100", b"FIELDS", b"a", b"n",
];

fn wait_index_ready(s: &mut std::net::TcpStream) -> Vec<u8> {
    for _ in 0..100 {
        let reply = cmd(s, QUERY);
        if !reply.starts_with(b"-INDEXBUILDING") {
            return reply;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("index never left BUILDING");
}

#[test]
fn cold_table_digest_backfill_and_hydration_match_the_hot_twin() {
    let hot = Server::start(None);
    let cold = Server::start(Some(256 * 1024));
    let mut hc = hot.connect();
    let mut cc = cold.connect();

    // Phase A — identical rows on both servers.
    write_rows(&mut hc);
    write_rows(&mut cc);

    // Phase B — drain demotion to a stable fixpoint on the tiered twin.
    let cold_keys = wait_cold_stable(&mut cc);
    assert!(cold_keys > 8, "need more cold rows than shards to prove batching, got {cold_keys}");

    // Phase C — PREFIX.DIGEST equality over the mostly-cold prefix,
    // with zero promotions (the peek_scope adoption).
    let dh = cmd(&mut hc, &[b"PREFIX.DIGEST", b"row:"]);
    let dc = cmd(&mut cc, &[b"PREFIX.DIGEST", b"row:"]);
    assert_eq!(dc, dh, "digest over cold rows must equal the hot twin");
    // One rested snapshot for all three: an equality between two gauges
    // read by two round trips is an equality across two moments, and each
    // read refreshes the answering shard before summing the others.
    let g = common::snapshot_at_rest("tier gauges", || {
        info_gauges(&mut cc, &["promotions_total", "peek_preads_total", "cold_keys"])
    });
    assert_eq!(g[0], 0, "digest must not promote");
    assert_eq!(
        g[1],
        g[2],
        "digest: one read per cold row"
    );

    // Phase D — IDX.CREATE AFTER the rows went cold: the backfill runs
    // entirely over the no-promote peek.
    let create: &[&[u8]] = &[
        b"IDX.CREATE", b"byn", b"ON", b"PREFIX", b"row:", b"FIELD", b"n", b"TYPE", b"i64",
        b"KIND", b"range",
    ];
    assert_eq!(cmd(&mut hc, create), b"+OK\r\n");
    assert_eq!(cmd(&mut cc, create), b"+OK\r\n");
    let qh = wait_index_ready(&mut hc);
    let _ = wait_index_ready(&mut cc); // backfill done; hydrations counted below
    assert_eq!(
        common::at_rest("promotions_total", || info_gauge(&mut cc, "promotions_total")),
        0,
        "neither backfill nor hydration may promote"
    );
    assert_eq!(info_gauge(&mut cc, "cold_keys"), cold_keys, "rows stay cold");

    // Phase E — ONE measured query on the ready index: reply
    // byte-identical to the hot twin; one read per cold ROW (the page
    // hydrates every row once; 2 fields ≠ 2 reads), coalesced into at
    // most one batch per shard.
    // BOTH ends of a delta have to be at rest. Resting only the second
    // read swaps one error for another: a stale `pre` is too LOW, so the
    // difference comes out too HIGH — 54 against 52 where the unrested
    // version had read 26.
    // BOTH ends of a delta have to be at rest, and both gauges have to
    // come from the same snapshot: resting only the second read swaps one
    // error for another — a stale `pre` is too LOW, so the difference came
    // out too HIGH (54 against 52, where the unrested version read 26).
    const GAUGES: &[&str] = &["peek_preads_total", "batch_submissions_total", "cold_keys"];
    let pre = common::snapshot_at_rest("tier gauges", || info_gauges(&mut cc, GAUGES));
    let qc = cmd(&mut cc, QUERY);
    assert_eq!(qc, qh, "IDX.QUERY FIELDS over cold rows must be byte-identical to the hot twin");
    let post = common::snapshot_at_rest("tier gauges", || info_gauges(&mut cc, GAUGES));
    let preads = post[0] - pre[0];
    let subs = post[1] - pre[1];
    assert_eq!(preads, post[2], "hydration: preads == cold rows, not rows × fields");
    assert!((1..=8).contains(&subs), "one batch per shard page, got {subs}");
    assert!(subs < preads, "batching must coalesce rows: {subs} submissions for {preads} reads");

    // Phase F — IDX.VERIFY runs its row recheck through the peek
    // scope: identical drift stats on both twins, still no promotions.
    let vh = cmd(&mut hc, &[b"IDX.VERIFY", b"byn"]);
    let vc = cmd(&mut cc, &[b"IDX.VERIFY", b"byn"]);
    assert_eq!(vc, vh, "IDX.VERIFY over cold rows must match the hot twin");
    assert_eq!(
        common::at_rest("promotions_total", || info_gauge(&mut cc, "promotions_total")),
        0
    );
}
