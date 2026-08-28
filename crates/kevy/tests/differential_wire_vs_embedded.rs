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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

mod common;

static START_GATE: Mutex<()> = Mutex::new(());

fn argv(cmd: &str) -> Vec<Vec<u8>> {
    cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect()
}


/// How many bytes of `buf` one complete RESP reply occupies, or `None` if
/// more is needed. Covers what this server emits: simple string, error,
/// integer, bulk string (and its null form), array (nested, and its null
/// form), plus RESP3's `_`, `#` and `,`.
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

    fn wire(&self) -> common::Wire {
        let sock = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        common::Wire::new(sock)
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

/// Verbs whose short call answers with their own usage line rather than
/// Redis's arity sentence. Every one is kevy-only — there is no Redis
/// counterpart to be compatible with — and for a declaration verb taking
/// eleven arguments a usage line is the more useful answer. The ledger is
/// EXACT: a verb that starts, or stops, doing this fails below by name.
const OWN_USAGE_LINE: &[&str] = &[
    "FAILOVER",
    "IDX.CREATE",
    "IDX.DROP",
    "REPL.WAIT",
    "TABLE.DECLARE",
    "TABLE.DROP",
    "TABLE.ENSURE",
    "TABLE.REPLACE",
    "TABLE.VERIFY",
    "VIEW.CREATE",
    "VIEW.DROP",
];

/// Every documented verb, called with one argument fewer than it declares,
/// must answer in Redis's words — and must never report itself unknown.
///
/// This was a hand-written list of fourteen guarded verbs, which is how the
/// defect it was written for got found: twelve of those fourteen reported a
/// command that exists as one that does not. But a hand list finds what
/// someone thought to write down. Once `kevy_resp::verb_arity` put the
/// arity column where a test outside `kevy` could read it, the list became
/// a sweep over all of them — and the sweep found four things in its first
/// run that the fourteen never had:
///
///   * `XPENDING k` PANICKED the shard thread. The extended parse reads
///     `args[3]` unchecked and nothing rejected a two-argument call, so any
///     client could crash a shard.
///   * `DEL` / `EXISTS` / `UNLINK` with no key routed to the multi-key
///     fan-out and answered `:0` — an empty delete — where redis 8.10.1
///     answers with the arity sentence. The embedded facade mirrored that
///     `:0` on purpose, with a comment saying to, so the differential
///     harness saw two surfaces agreeing. Agreement on a wrong answer is
///     still agreement; it took the Redis oracle to say which was right.
///   * `SRANDMEMBER` declared -3 where redis declares -2, so the valid
///     `SRANDMEMBER key` was accepted on the wire and REFUSED inside MULTI.
///     See the cell below.
///   * `XREAD a b` answered "syntax error" where redis answers with the
///     arity sentence, keeping "syntax error" for a call long enough to
///     parse but shaped wrong.
///
/// Only the too-FEW direction is swept, deliberately: a call below the
/// minimum cannot reach a handler, so this can never execute anything.
#[test]
fn every_documented_verb_refuses_a_short_call_in_redis_words() {
    use kevy_resp::verb_arity::{VERB_ARITY, arity_of};
    let server = Server::start_single_shard();
    let mut wire = server.wire();
    let registered: std::collections::HashSet<&str> = OWN_USAGE_LINE.iter().copied().collect();

    let short_call = |name: &str| {
        let min = arity_of(name).map_or(0, |a| a.unsigned_abs() as usize);
        let mut parts = vec![name.as_bytes().to_vec()];
        for _ in 0..min.saturating_sub(2) {
            parts.push(b"x".to_vec());
        }
        parts
    };

    let (mut probed, mut skipped) = (0usize, 0usize);
    let (mut unknown, mut other) = (Vec::new(), Vec::new());
    for (name, arity) in VERB_ARITY {
        if arity.unsigned_abs() < 2 {
            skipped += 1; // a call cannot carry fewer parts than the verb
            continue;
        }
        probed += 1;
        let r = String::from_utf8_lossy(&wire.call(&short_call(name))).to_string();
        if r.contains("unknown command") {
            unknown.push(format!("{name} -> {}", r.trim()));
        } else if !r.contains("wrong number of arguments") && !registered.contains(name) {
            other.push(format!("{name} -> {}", r.trim().chars().take(90).collect::<String>()));
        }
    }

    // A floor: a sweep that probed nothing passes every assertion below.
    assert!(
        probed >= 150,
        "only {probed} verbs probed ({skipped} skipped) — VERB_ARITY did not load"
    );

    // Controls, so a green here cannot come from the server answering
    // everything the same way.
    let nonexistent = String::from_utf8_lossy(&wire.call(&argv("TOTALLY.NOT.A.COMMAND"))).to_string();
    assert!(nonexistent.contains("unknown command"), "a real unknown stays unknown: {nonexistent}");
    let good = String::from_utf8_lossy(&wire.call(&argv("IDX.LIST"))).to_string();
    assert!(
        !good.contains("wrong number of arguments") && !good.contains("unknown command"),
        "a correct call must not become an arity error: {good}"
    );

    assert!(
        unknown.is_empty(),
        "{} documented verb(s) answer a short call with \"unknown command\" rather \
         than the arity sentence: {unknown:?}",
        unknown.len()
    );
    assert!(
        other.is_empty(),
        "{} verb(s) answer a short call with neither the arity sentence nor a \
         registered usage line: {other:?}",
        other.len()
    );

    let healed: Vec<&str> = OWN_USAGE_LINE
        .iter()
        .copied()
        .filter(|n| String::from_utf8_lossy(&wire.call(&short_call(n))).contains("wrong number of arguments"))
        .collect();
    assert!(
        healed.is_empty(),
        "{healed:?} now answer in Redis's words — drop them from OWN_USAGE_LINE so \
         the ledger stays exact"
    );
}

/// `SRANDMEMBER key` is a valid call. It was accepted on the wire and
/// REFUSED inside MULTI, because the transaction queue checks the declared
/// arity and VERB_META declared -3 where redis 8.10.1 declares -2. One
/// wrong number, and the same command meant two different things depending
/// on whether a transaction was open.
#[test]
fn a_valid_call_is_not_refused_by_the_transaction_queue() {
    let server = Server::start_single_shard();
    let mut w = server.wire();
    let direct = String::from_utf8_lossy(&w.call(&argv("SRANDMEMBER k"))).to_string();
    assert!(!direct.contains("wrong number of arguments"), "direct call refused: {direct}");
    let multi = String::from_utf8_lossy(&w.call(&argv("MULTI"))).to_string();
    assert!(multi.starts_with("+OK"), "MULTI: {multi}");
    let queued = String::from_utf8_lossy(&w.call(&argv("SRANDMEMBER k"))).to_string();
    assert!(queued.starts_with("+QUEUED"), "the queue refused a call the wire accepts: {queued}");
    let _ = w.call(&argv("DISCARD"));
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
