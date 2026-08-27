//! The differential harness, over the wire.
//!
//! `differential_server_vs_embedded.rs` compares two in-process
//! dispatchers and reaches 66 of 70 commands. Three of the four it cannot
//! settle are the same shape: `cmd_resolve.rs` routes IDX.QUERY, IDX.LIST
//! and VIEW.LIST to `Route::Extension`, which is a scatter-gather across
//! every shard (`kevy-rt/src/exec_build.rs:101`) rather than a call the
//! bare `KevyCommands` dispatcher can make. The server implements them; the
//! harness could not reach them.
//!
//! This one can, because it drives a real server over RESP. Two decisions
//! make the comparison mean something:
//!
//! **One shard.** With `shards(1)` the scatter-gather degenerates to a
//! single participant, so the server and the single-process embedded facade
//! are answering the same question. At eight shards a gathered result may
//! legitimately order differently, and a byte comparison would report that
//! as a divergence when it is arithmetic.
//!
//! **A length-aware reader.** The sibling e2e tests sleep 30 ms and read
//! once, which is fine when a test knows what it asked for. Here a short
//! read would manufacture a divergence out of nothing, so the reply is
//! parsed and read until complete.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static START_GATE: Mutex<()> = Mutex::new(());

fn argv(cmd: &str) -> Vec<Vec<u8>> {
    cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect()
}

fn encode(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

/// How many bytes of `buf` one complete RESP reply occupies, or `None` if
/// more is needed. Covers what this server emits: simple string, error,
/// integer, bulk string (and its null form), array (nested, and its null
/// form), plus RESP3's `_`, `#` and `,`.
fn reply_len(buf: &[u8]) -> Option<usize> {
    fn line_end(buf: &[u8], from: usize) -> Option<usize> {
        buf.get(from..)?
            .windows(2)
            .position(|w| w == b"\r\n")
            .map(|p| from + p + 2)
    }
    let tag = *buf.first()?;
    let head = line_end(buf, 1)?;
    match tag {
        b'+' | b'-' | b':' | b'_' | b'#' | b',' => Some(head),
        b'$' => {
            let n: i64 = std::str::from_utf8(&buf[1..head - 2]).ok()?.parse().ok()?;
            if n < 0 {
                return Some(head);
            }
            let end = head + n as usize + 2;
            (buf.len() >= end).then_some(end)
        }
        b'*' | b'~' | b'>' => {
            let n: i64 = std::str::from_utf8(&buf[1..head - 2]).ok()?.parse().ok()?;
            if n < 0 {
                return Some(head);
            }
            let mut at = head;
            for _ in 0..n {
                at += reply_len(buf.get(at..)?)?;
            }
            Some(at)
        }
        b'%' => {
            let n: i64 = std::str::from_utf8(&buf[1..head - 2]).ok()?.parse().ok()?;
            let mut at = head;
            for _ in 0..(n.max(0) * 2) {
                at += reply_len(buf.get(at..)?)?;
            }
            Some(at)
        }
        _ => None,
    }
}

struct Wire {
    sock: std::net::TcpStream,
    buf: Vec<u8>,
}

impl Wire {
    fn call(&mut self, parts: &[Vec<u8>]) -> Vec<u8> {
        self.sock.write_all(&encode(parts)).expect("write");
        loop {
            if let Some(n) = reply_len(&self.buf) {
                return self.buf.drain(..n).collect();
            }
            let mut chunk = [0u8; 65536];
            let n = self.sock.read(&mut chunk).expect("read");
            assert!(n > 0, "server closed mid-reply");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// One shard, on purpose — see the module docs.
    fn start_single_shard() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-diffwire-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, d) = (stop.clone(), dir.clone());
        let handle = std::thread::spawn(move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
                .bind([127, 0, 0, 1], port)
                .shards(1)
                .with_data_dir(d)
                .run(st)
                .unwrap();
        });
        kevy_testnet::assert_listening(port, "the differential server");
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn wire(&self) -> Wire {
        let sock = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        Wire { sock, buf: Vec::new() }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The extension surface the in-process harness could not reach, plus
/// enough ordinary traffic to prove the wire path itself agrees.
const CORPUS: &[&str] = &[
    "PING",
    "SET k v",
    "GET k",
    "HSET h f1 v1 f2 v2",
    "HGET h f1",
    "ZADD z 1 one 2 two",
    "ZRANGE z 0 -1 WITHSCORES",
    // the extension route — Route::Extension in cmd_resolve.rs
    "IDX.LIST",
    "IDX.CREATE t SCHEMA name TEXT age NUMERIC",
    "IDX.LIST",
    "IDX.INFO t",
    "IDX.CREATE t SCHEMA name TEXT",
    "IDX.QUERY t *",
    "IDX.COUNT t *",
    "IDX.EXPLAIN t",
    "IDX.VERIFY t",
    "IDX.REBUILD t",
    "IDX.DROP t",
    "IDX.LIST",
    "IDX.INFO t",
    "VIEW.LIST",
    "VIEW.INFO nope",
    // errors, where a second implementation is most likely to differ
    "IDX.CREATE",
    "IDX.QUERY",
    "IDX.DROP nosuch",
    "TOTALLY.NOT.A.COMMAND",
];

/// Divergences that are correct, each with the reason. Written after the
/// first run, never before.
///
/// The first run had six. Two were an error-wording split that has since
/// been closed — the server said "unknown command 'IDX.QUERY'" for a
/// wrong-arity call while the facade said "bad arguments", and both now say
/// what Redis says. One was a defect in the server that the probe below
/// generalised to twelve verbs.
///
/// These three are a capability gap, not a defect, and naming them is the
/// point of the register.
const EXPECTED: &[(&str, &str)] = &[
    (
        "IDX.EXPLAIN t",
        "The server implements IDX.EXPLAIN (cmd_resolve.rs:185 routes it to          Route::Extension); the embedded facade's dispatch table does not          carry the verb at all. Its IDX set is {ADVISE, COUNT, CREATE, DROP,          LIST, QUERY}; the server's extension set is {COUNT, EXPLAIN, LIST,          QUERY, REBUILD, VERIFY}. So each has one the other lacks — this is          two surfaces that drifted, not one implemented twice.",
    ),
    (
        "IDX.VERIFY t",
        "Same gap: server-only (cmd_resolve.rs:188), absent from the          facade's dispatch table.",
    ),
    (
        "IDX.REBUILD t",
        "Same gap: server-only (cmd_resolve.rs:186), absent from the          facade's dispatch table.",
    ),
];

fn render(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace("\r\n", "\\r\\n").chars().take(200).collect()
}

#[test]
fn embedded_answers_what_the_wire_answers() {
    let server = Server::start_single_shard();
    let mut wire = server.wire();

    let dir = kevy_tmpdir::TmpDir::new("diff-wire-embedded");
    let cfg = kevy_embedded::Config::default().with_persist(dir.path().to_str().unwrap());
    let embedded = kevy_embedded::Store::open(cfg).expect("open embedded");

    let expected: std::collections::BTreeSet<&str> =
        EXPECTED.iter().map(|(c, _)| *c).collect();
    let (mut agreed, mut diverged) = (0usize, Vec::new());

    for cmd in CORPUS {
        let a = argv(cmd);
        let w = wire.call(&a);
        let mut e = Vec::new();
        embedded.dispatch_argv(&a, &mut e);
        if w == e {
            agreed += 1;
        } else {
            diverged.push((*cmd, w, e));
        }
    }

    let unnamed: Vec<_> =
        diverged.iter().filter(|(c, _, _)| !expected.contains(c)).collect();

    println!(
        "differential(wire): {} of {} agree byte-for-byte; {} diverge \
         ({} named, {} not)",
        agreed,
        CORPUS.len(),
        diverged.len(),
        diverged.len() - unnamed.len(),
        unnamed.len()
    );
    for (cmd, w, e) in &diverged {
        let tag = if expected.contains(cmd) { "named" } else { "UNNAMED" };
        println!("  [{tag}] {cmd}");
        println!("      wire:     {}", render(w));
        println!("      embedded: {}", render(e));
    }

    assert_eq!(agreed + diverged.len(), CORPUS.len(), "the corpus did not run");
    assert!(
        unnamed.is_empty(),
        "{} command(s) diverge without a stated reason",
        unnamed.len()
    );
}

/// Every verb `cmd_resolve.rs` guards by arity, called with the WRONG
/// arity. The guard does not match, the arm falls through, and the
/// default arm does not know these verbs — so the server reports a
/// command that exists as one that does not.
const ARITY_PROBE: &[&str] = &[
    "IDX.QUERY t",
    "IDX.EXPLAIN",
    "IDX.REBUILD",
    "IDX.COUNT t",
    "IDX.VERIFY",
    "IDX.LIST extra",
    "VIEW.QUERY",
    "VIEW.LIST extra",
    "VIEW.VERIFY",
    "VIEW.REBUILD",
    "VIEW.EXPLAIN",
    "TABLE.LIST extra",
    "TABLE.VERIFY",
    "PREFIX.DIGEST",
];

#[test]
fn a_known_verb_with_wrong_arity_is_not_reported_as_unknown() {
    let server = Server::start_single_shard();
    let mut wire = server.wire();
    let mut wrong = Vec::new();
    for cmd in ARITY_PROBE {
        let r = String::from_utf8_lossy(&wire.call(&argv(cmd))).to_string();
        if r.contains("unknown command") {
            wrong.push((*cmd, r.trim().to_string()));
        }
    }
    println!("arity probe: {} of {} report a real verb as unknown",
             wrong.len(), ARITY_PROBE.len());
    for (c, r) in &wrong {
        println!("  {c}  ->  {r}");
    }
    // The control: a verb that genuinely does not exist must still be
    // reported as unknown, and a known verb at the RIGHT arity must not be
    // turned into an arity error by this change.
    let unknown = String::from_utf8_lossy(&wire.call(&argv("TOTALLY.NOT.A.COMMAND"))).to_string();
    assert!(unknown.contains("unknown command"), "a real unknown stays unknown: {unknown}");
    let good = String::from_utf8_lossy(&wire.call(&argv("IDX.LIST"))).to_string();
    assert!(
        !good.contains("wrong number of arguments") && !good.contains("unknown command"),
        "a correct call must not become an arity error: {good}"
    );

    assert!(
        wrong.is_empty(),
        "{} guarded verb(s) answer a wrong-arity call with \"unknown command\" \
         instead of \"wrong number of arguments\"",
        wrong.len()
    );
}

/// Arity is declared in one place and read in two — and one of them cannot
/// reach it.
///
/// `verb_meta` carries every verb's arity and lives in `kevy`, which is
/// cement. `kevy-embedded` is steel and may not depend on it, so the facade
/// restates the numbers it needs as literals in its own entry checks. That
/// is exactly how the two surfaces came to answer the same wrong-arity call
/// in two different sentences, and restating a constant is a drift waiting
/// to happen a second time.
///
/// Unifying the two registries is a real change — `kevy-resp`'s `OpSpec`
/// (166 verbs, classification and surfaces) and `verb_meta` (191 verbs,
/// documentation and arity) overlap on 152 and are complementary on the
/// rest, and the first is guarded by exhaustive parity tests. Not made
/// here.
///
/// What is made here is the check that makes the drift impossible to ship
/// unnoticed: for every verb `ops_table` says exists on BOTH surfaces, call
/// it with too few arguments on each and require the same answer. A
/// behavioural check rather than a comparison of constants, so it catches a
/// divergence in the number and in the sentence alike.
#[test]
fn both_surfaces_refuse_a_short_call_the_same_way() {
    use kevy_resp::ops_table::{spec, surface};

    let server = Server::start_single_shard();
    let mut wire = server.wire();
    let dir = kevy_tmpdir::TmpDir::new("diff-arity");
    // The change feed is off by default in the facade and on in the server.
    // Left unequal, FEED.TAIL answers "feed: Disabled" here and an arity
    // error there — a difference in how the harness was set up, reported as
    // if it were a difference in the code.
    let cfg = kevy_embedded::Config::default()
        .with_persist(dir.path().to_str().unwrap())
        .with_feed(64 * 1024 * 1024);
    let embedded = kevy_embedded::Store::open(cfg).expect("open embedded");

    let both: Vec<&'static str> = kevy_resp::ops_table::ops_with(surface::SERVER)
        .into_iter()
        .filter(|n| spec(n).is_some_and(|s| s.surfaces & surface::ESTORE != 0))
        .collect();
    assert!(
        both.len() > 50,
        "only {} verbs on both surfaces — the selector is broken, not the engine bare",
        both.len()
    );

    let mut differ = Vec::new();
    for name in &both {
        // One argument: the verb alone. Every verb here takes at least one
        // operand, so this is a short call for all of them and touches no
        // state on either side.
        let a = vec![name.as_bytes().to_vec()];
        let w = String::from_utf8_lossy(&wire.call(&a)).trim().to_string();
        let mut e = Vec::new();
        embedded.dispatch_argv(&a, &mut e);
        let e = String::from_utf8_lossy(&e).trim().to_string();
        if w != e {
            differ.push((*name, w, e));
        }
    }

    // Different signatures, not drift: the server is sharded and takes the
    // shard as an argument (`FEED.TAIL shard`, arity 2), while the
    // single-process facade has no shard to name and takes none. The
    // difference is in verb_meta's own `syntax` field for both verbs.
    const DIFFERENT_SIGNATURE: &[&str] = &["FEED.TAIL", "FEED.READ"];

    println!(
        "arity parity: {} of {} verbs on both surfaces refuse a short call identically",
        both.len() - differ.len(),
        both.len()
    );
    for (n, w, e) in differ.iter().take(20) {
        println!("  {n}\n      wire:     {w}\n      embedded: {e}");
    }
    if differ.len() > 20 {
        println!("  … and {} more", differ.len() - 20);
    }

    let unexplained: Vec<&str> = differ
        .iter()
        .map(|(n, _, _)| *n)
        .filter(|n| !DIFFERENT_SIGNATURE.contains(n))
        .collect();
    assert!(
        unexplained.is_empty(),
        "{} verb(s) refuse a short call differently on the two surfaces \
         without a stated reason: {unexplained:?}",
        unexplained.len()
    );
}
