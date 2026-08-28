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
    // ── the rest of the facade's dispatch table ──
    // 60 of its 130 verbs were driven before this; the two surfaces had
    // never been compared byte-for-byte on the other 70.
    //
    // ORDER MATTERS. A verb only ONE side implements must not mutate
    // shared state, or every later read diverges for a reason that is not
    // about that read. The first pass of this block put COPY, LINSERT and
    // HINCRBYFLOAT in the middle, and six later commands — EXISTS, KEYS,
    // LREM, HSCAN, PREFIX.DIGEST, PREFIX.STATS — reported a divergence
    // that was really the corpus's own doing. The F3 verbs now come last
    // and on their own keys.
    "MSET s1 a s2 b s3 c",
    "MGET s1 s2 nope",
    "GETSET s1 z",
    "GETDEL s3",
    "INCRBYFLOAT f 1.5",
    "DECRBY n 3",
    "INCRBYFLOAT f -0.5",
    "APPEND s1 tail",
    "STRLEN s1",
    "EXISTS s1 s2 nope",
    // hashes
    "HSET h2 f1 v1 f2 v2 f3 v3",
    "HKEYS h2",
    "HVALS h2",
    "HEXISTS h2 f1",
    "HEXISTS h2 nope",
    "HSCAN h2 0",
    "HDEL h2 f3",
    // Per-field TTL: same rule as the key-level one below. HPERSIST's own
    // answer proves the deadline was there (1 = removed one), and the
    // reads that follow are -1 (no TTL) and -2 (no field) — answers about
    // existence, not about the clock.
    "HEXPIRE h2 100 FIELDS 1 f1",
    "HPERSIST h2 FIELDS 1 f1",
    "HTTL h2 FIELDS 1 f1",
    "HPTTL h2 FIELDS 1 f1",
    "HPTTL h2 FIELDS 1 nosuchfield",
    // lists
    "RPUSH L a b c d e",
    "LSET L 0 A",
    "LREM L 1 A",
    "LTRIM L 0 2",
    "LRANGE L 0 -1",
    // expiry
    //
    // The verbs are driven; the values COMPARED are only ones that cannot
    // move. `Store::pttl` reads a live nanosecond clock on every call
    // (`let now = now_ns()`), so a remaining-time answer is a function of
    // WHEN it was asked — and this harness asks the wire first and the
    // facade second. Two calls a few hundred microseconds apart land in
    // the same millisecond on an idle machine and need not on a loaded
    // one. Comparing them byte-for-byte asserts they happened at the same
    // instant, which is the same mistake RANDOMKEY and TIME are excluded
    // for, made in the corpus that excludes them.
    //
    // So: set deadlines, then compare -1 (no expiry) and -2 (no key),
    // which are answers about existence rather than about the clock.
    "EXPIREAT s1 99999999999",
    "PEXPIREAT s2 99999999999000",
    "PEXPIRE s1 100000",
    "PERSIST s1",
    "TTL s1",
    "PTTL s1",
    "TTL nosuchkey",
    "PTTL nosuchkey",
    // keyspace + admin
    "KEYS s*",
    "ECHO hello",
    "PUBLISH ch nobody-listening",
    "PREFIX.DIGEST s",
    "PREFIX.STATS s",
    "IDX.ADVISE",
    "FEED.SHARDS",
    // sets
    "SADD A a b c",
    "SADD B b c d",
    "SDIFF A B",
    "SINTER A B",
    "SUNION A B",
    "SDIFFSTORE Sd A B",
    "SINTERSTORE Si A B",
    "SUNIONSTORE Su A B",
    "SCARD Su",
    "SPOP A 0",
    "SETNX newkey v",
    "SETNX newkey again",
    // sorted sets
    "ZADD Z 1 one 2 two 3 three",
    "ZADD Y 2 two 3 three 4 four",
    "ZREVRANGE Z 0 -1",
    "ZREVRANGEBYSCORE Z 3 1",
    "ZINTERCARD 2 Z Y",
    "ZINTERSTORE Zi 2 Z Y",
    "ZUNIONSTORE Zu 2 Z Y",
    "ZDIFFSTORE Zd 2 Z Y",
    "ZSCAN Z 0",
    "ZPOPMIN Z",
    "ZPOPMIN.BELOW Z 3",
    "ZREMRANGEBYRANK Y 0 0",
    "ZREMRANGEBYSCORE Y 4 4",
    "ZRANGE Y 0 -1 WITHSCORES",
    // lists + keyspace
    "RPOP L",
    "TOUCH s1 nope",
    "UNLINK f3copy",
    "RENAMENX s2 s2renamed",
    "SCAN 0 COUNT 100",
    "TABLE.LIST",
    "VIEW.QUERY nosuch",
    "HPEXPIRE h2 100000 FIELDS 1 f2",
    "HPEXPIREAT h2 99999999999000 FIELDS 1 f2",
    "HPERSIST h2 FIELDS 1 f2",
    // ── the rest of the shared surface ──
    // Every verb `ops_table` says both surfaces carry, driven with real
    // arguments so the ANSWER is compared and not merely the refusal.
    // The register below refuses to let this block shrink again.
    //
    // Own keys, and placed after every read that walks the keyspace
    // (KEYS, SCAN, PREFIX.*), so what these add cannot move an earlier
    // answer. The prefix is `w`, not `s`, for the same reason.
    "SET wnum 10",
    "TYPE wnum",
    "TYPE nosuchkey",
    "INCR wnum",
    "INCRBY wnum 5",
    "DECR wnum",
    "GET wnum",
    "HSET wh f1 v1 f2 2",
    "HGETALL wh",
    "HLEN wh",
    "HMGET wh f1 f2 nope",
    "HINCRBY wh f2 3",
    "HSETNX wh f2 ignored",
    "HSETNX wh f3 kept",
    "HGETALL wh",
    "RPUSH wl a b c",
    "LLEN wl",
    "LINDEX wl 0",
    "LINDEX wl 99",
    "LPUSH wl z",
    "LPOP wl",
    "SADD wset a b c",
    "SISMEMBER wset a",
    "SISMEMBER wset nope",
    "SMEMBERS wset",
    // SRANDMEMBER draws from each surface's own generator, so what it
    // returns is defined as a SET and only once the count reaches the
    // cardinality — `set_read.rs` shuffles even then. Compared as a
    // multiset, via UNORDERED below.
    "SRANDMEMBER wset 3",
    "SREM wset a",
    "SCARD wset",
    "ZADD wz 1 one 2 two 3 three",
    "ZCARD wz",
    "ZCOUNT wz 1 2",
    "ZSCORE wz two",
    "ZSCORE wz nope",
    "ZRANK wz two",
    "ZRANK wz nope",
    "ZINCRBY wz 5 two",
    "ZRANGEBYSCORE wz 2 7",
    "ZREM wz one",
    "ZCARD wz",
    // the declarative surface
    "TABLE.DECLARE wt PREFIX wrow: PK id COLUMN id i64 COLUMN city str INDEX city range",
    "TABLE.LIST",
    "TABLE.VERIFY wt",
    "TABLE.ENSURE wt PREFIX wrow: PK id COLUMN id i64 COLUMN city str INDEX city range",
    "TABLE.REPLACE wt PREFIX wrow: PK id COLUMN id i64 COLUMN city str COLUMN age i64 INDEX city range",
    "VIEW.CREATE wv QUERY wt.city EQ tokyo ORDER BY wt.city",
    "VIEW.LIST",
    "VIEW.INFO wv",
    "VIEW.DROP wv",
    "TABLE.DROP wt",
    // keyspace
    "SET wren v",
    "RENAME wren wren2",
    "GET wren2",
    "EXPIRE wren2 100000",
    "PERSIST wren2",
    "TTL wren2",
    "DEL wren2 nosuchkey",
    "EXISTS wren2",
    // ── the branches, not just the happy path ──
    // The verbs above were driven once each, which reaches a handler and
    // nothing inside it. `deadgate` said so on the first CI run after
    // they were wired: eleven symbols joined the never-executed set,
    // `cmd_getex` with 52 regions and `cmd_bitpos` with 26 — the option
    // forms and the refusals, none of them touched.
    //
    // Driving them HERE rather than asserting bytes in a test of their
    // own is the stronger check: two independent implementations answer
    // each line and must agree, so there is no expected reply for me to
    // write down wrong.
    "SET xs hello-world",
    "GETRANGE xs 0 4",
    "GETRANGE xs -5 -1",
    "GETRANGE xs 99 200",
    "GETRANGE xs a 4",
    "GETRANGE xs 0",
    "GETRANGE",
    "SETRANGE xs 5 _____",
    "SETRANGE xs 20 tail",
    "SETRANGE xs -1 x",
    "SETRANGE xs abc x",
    "SETRANGE xs 0",
    "GET xs",
    "GETEX xs",
    "GETEX xs EX 1000",
    "GETEX xs PX 1000000",
    "GETEX xs ex 1000",
    "GETEX xs XX 1000",
    "GETEX xs EX 0",
    "GETEX xs EX -5",
    "GETEX xs EX abc",
    "GETEX xs EX 1000 EXTRA",
    "GETEX xs",
    "GETEX xnosuch",
    "PERSIST xs",
    "SETBIT xb 7 1",
    "SETBIT xb 7 0",
    "SETBIT xb 100 1",
    "SETBIT xb -1 1",
    "SETBIT xb abc 1",
    "SETBIT xb 7 2",
    "SETBIT xb 7",
    "GETBIT xb 100",
    "GETBIT xb 99999",
    "GETBIT xb -1",
    "GETBIT xb",
    "BITCOUNT xb",
    "BITCOUNT xb 0 -1",
    "BITCOUNT xb 0 0",
    "BITCOUNT xb a b",
    "BITCOUNT xb 0 1 2",
    "BITCOUNT",
    "BITPOS xb 1",
    "BITPOS xb 0",
    "BITPOS xb 1 0",
    "BITPOS xb 1 0 -1",
    "BITPOS xb 1 5 6",
    "BITPOS xb 2",
    "BITPOS xb 1 a",
    "BITPOS xb 1 0 a",
    "BITPOS xb",
    "BITPOS xb 1 0 1 2",
    "RPUSH xl a b c",
    "LINSERT xl BEFORE b X",
    "LINSERT xl AFTER b Y",
    "LINSERT xl before a Z",
    "LINSERT xl BEFORE nosuchpivot Q",
    "LINSERT xl SIDEWAYS b Q",
    "LINSERT xl BEFORE b",
    "LINSERT xnosuchlist BEFORE b X",
    "LRANGE xl 0 -1",
    "HSET xh f 1",
    "HINCRBYFLOAT xh f 1.5",
    "HINCRBYFLOAT xh f -0.25",
    "HINCRBYFLOAT xh f abc",
    "HINCRBYFLOAT xh f",
    "HINCRBYFLOAT xh newfield 2.5",
    "ZADD xz 1 one 2 two 3 three",
    "ZREVRANGE xz 0 -1",
    "ZREVRANGE xz 0 -1 WITHSCORES",
    "ZREVRANGE xz 0 0",
    "ZREVRANGE xz -2 -1",
    "ZREVRANGE xz 5 10",
    "ZREVRANGE xz 2 1",
    "ZREVRANGE xz 0 -1 SCORES",
    "ZREVRANGE xz a b",
    "ZREVRANGE xz 0",
    "ZREVRANGE xnosuchzset 0 -1",
    "TOUCH xs xl nosuchkey",
    "TOUCH",
    "COPY xs xcopy",
    "COPY xs xcopy",
    "COPY xs xcopy REPLACE",
    "COPY xs xcopy REPLACED",
    "COPY xs xs",
    "COPY xnosuch xdst",
    "COPY xs",
    "GET xcopy",
    // The error arm each of these has and none of the lines above
    // reaches: the store refusing because the key holds another type.
    // `deadgate` counted them one by one.
    "GETRANGE xl 0 1",
    "SETRANGE xl 0 x",
    "GETEX xl EX 100",
    "SETBIT xl 0 1",
    "GETBIT xl 0",
    "BITCOUNT xl",
    "BITCOUNT xl 0 1",
    "BITPOS xl 1",
    "BITPOS xl 1 0 1",
    "LINSERT xs BEFORE a b",
    "HINCRBYFLOAT xl f 1",
    "ZREVRANGE xl 0 -1",
    "ZREVRANGE xl 0 -1 WITHSCORES",
    // ── F3: implemented in the facade, absent from the RESP dispatch ──
    // Registered in `kevy_resp::ops_table::KNOWN_GAPS`, and the check
    // below reads that ledger rather than restating it. Own keys, last,
    // so their divergence cannot become anyone else's.
    "GETRANGE s1 0 0",
    "SETRANGE f3k 1 X",
    "GETEX s1",
    "COPY s1 f3copy",
    "SETBIT f3bits 7 1",
    "GETBIT f3bits 7",
    "BITCOUNT f3bits",
    "BITPOS f3bits 1",
    "BITOP AND f3dest f3bits f3bits",
    "HINCRBYFLOAT f3h num 1.25",
    "LINSERT L BEFORE b X",
    // errors, where a second implementation is most likely to differ
    "IDX.CREATE",
    "IDX.QUERY",
    "IDX.DROP nosuch",
    "TOTALLY.NOT.A.COMMAND",
    // LAST, and only here: it empties the keyspace both surfaces have
    // been answering from, so anything after it would be comparing two
    // empty stores and proving nothing.
    "FLUSHALL",
    "DBSIZE",
    // RANDOMKEY is deliberately never driven: it answers with a key of
    // its own choosing, so a byte comparison between two implementations
    // asserts something neither promises. That is not a coverage hole,
    // and `CANNOT_COMPARE` below now holds the reason where a test can
    // check it rather than only where a reader can find it.
    //
    // This comment used to name TIME beside it, as "a pair a differential
    // harness cannot speak about". They were never a pair: TIME is not on
    // the server wire at all — `ops_table::KNOWN_GAPS` registers it as F3,
    // implemented in the store and the facade and unwired on RESP — so
    // this harness would not have compared it whatever the clock did. The
    // register found that by refusing the entry.
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
    (
        "TABLE.VERIFY wt",
        "A difference of WHEN, not of what. The server backfills an index \
         as a background job advanced one batch per runtime tick \
         (`index_runtime/row_apply.rs:202 advance_backfill`), and until it \
         finishes every read of that index answers -INDEXBUILDING — a \
         contract the error states in words, telling the caller to poll \
         IDX.LIST. The corpus asks on the line after TABLE.DECLARE, before \
         any tick has run. The single-process facade has no tick loop, so \
         its index is ready when it is declared and TABLE.VERIFY answers \
         with the stats. Both answers are correct for the surface that \
         gave them; the corpus cannot wait, so this is named rather than \
         driven differently.",
    ),
];

/// Verbs whose reply Redis defines no ORDER for: a set, or a cursor
/// walk. Both implementations iterate their own tables, so the order
/// differs — and differs run to run, which is how this was found:
/// `SINTER` agreed on one run of this cell and diverged on the next.
///
/// Comparing bytes there asserts an order neither side promises, so
/// these compare as multisets of their top-level elements. The ELEMENTS
/// must still match exactly, which is the part that means something.
///
/// `KEYS`, `HSCAN` and `ZSCAN` are the same class and are deliberately
/// NOT here: they agree byte for byte today, and if that stops being
/// true the cell should say so rather than have been excused in advance.
const UNORDERED: &[&str] =
    &["SINTER", "SUNION", "SDIFF", "SCAN", "SMEMBERS", "SRANDMEMBER"];

/// A canonical form for a reply whose order is not defined: every array
/// in it, at every depth, has its elements sorted.
///
/// `SCAN` needs the depth — its reply is `[cursor, [keys…]]`, and the
/// keys are the part that reorders. Sorting the outer pair as well is
/// harmless (both sides get the same treatment) and costs only the
/// ability to notice a reply that swapped its cursor for its keys, which
/// is not a thing either implementation could do.
fn canonical_unordered(reply: &[u8]) -> Option<Vec<u8>> {
    if reply.first()? != &b'*' {
        return Some(reply.to_vec());
    }
    let head = reply.windows(2).position(|w| w == b"\r\n")? + 2;
    let n: i64 = std::str::from_utf8(&reply[1..head - 2]).ok()?.parse().ok()?;
    if n < 0 {
        return Some(reply.to_vec());
    }
    let mut at = head;
    let mut parts = Vec::new();
    for _ in 0..n {
        let len = common::reply_len(&reply[at..])?;
        parts.push(canonical_unordered(&reply[at..at + len])?);
        at += len;
    }
    parts.sort();
    let mut out = format!("*{n}\r\n").into_bytes();
    for p in parts {
        out.extend_from_slice(&p);
    }
    Some(out)
}

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

    // Two registers, one of them already maintained elsewhere: the named
    // list above for capability gaps between the two surfaces, and
    // `ops_table::KNOWN_GAPS` for verbs the RESP dispatch does not carry
    // at all. Reading the second rather than copying it means a gap that
    // closes there stops being excused here, by construction.
    let mut expected: std::collections::BTreeSet<&str> =
        EXPECTED.iter().map(|(c, _)| *c).collect();
    let f3: std::collections::BTreeSet<&str> = kevy_resp::ops_table::KNOWN_GAPS
        .iter()
        .filter(|(_, surface, _)| surface & kevy_resp::ops_table::surface::SERVER != 0)
        .map(|(verb, _, _)| *verb)
        .collect();
    for cmd in CORPUS {
        if f3.contains(cmd.split(' ').next().unwrap_or("")) {
            expected.insert(cmd);
        }
    }
    let (mut agreed, mut diverged) = (0usize, Vec::new());

    for cmd in CORPUS {
        let a = argv(cmd);
        let w = wire.call(&a);
        let mut e = Vec::new();
        embedded.dispatch_argv(&a, &mut e);
        let verb = cmd.split(' ').next().unwrap_or("");
        let same = if w == e {
            true
        } else if UNORDERED.contains(&verb) {
            match (canonical_unordered(&w), canonical_unordered(&e)) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            }
        } else {
            false
        };
        if same {
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

    // Verbs the probe cannot speak about, collected rather than assumed:
    // see the arity gate in the loop.
    let mut complete_bare_call: Vec<&str> = Vec::new();
    let mut differ = Vec::new();
    for name in &both {
        // One argument: the verb alone. The premise is that this is a
        // SHORT call — and the arity column both surfaces now read says
        // for which verbs that holds. TIME is the first shared verb for
        // which it does not: its bare form is a complete call, so the
        // probe would be comparing two clock reads and calling the
        // difference a drift in how the two refuse.
        if kevy_resp::verb_arity::arity_ok(name, 1) == Some(true) {
            complete_bare_call.push(*name);
            continue;
        }
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
        "arity parity: {} of {} verbs on both surfaces refuse a short call \
         identically ({} skipped — a bare call is a complete one: {:?})",
        both.len() - differ.len() - complete_bare_call.len(),
        both.len(),
        complete_bare_call.len(),
        complete_bare_call
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

/// Verbs both surfaces carry that this corpus deliberately does not drive,
/// each with the reason a byte comparison cannot speak about it.
///
/// A differential has to compare something both sides promise. These
/// promise the opposite: an answer of their own choosing, or an answer to
/// a question the two surfaces are not asked in the same words.
const CANNOT_COMPARE: &[(&str, &str)] = &[
    (
        "RANDOMKEY",
        "answers with a key of its own choosing, so comparing bytes would \
         assert the two implementations chose alike",
    ),
    (
        "FEED.TAIL",
        "different signatures, not drift: the server is sharded and takes \
         the shard as an argument, the single-process facade has none — so \
         one corpus line cannot address both. Named for the same reason in \
         `both_surfaces_refuse_a_short_call_the_same_way`",
    ),
    ("FEED.READ", "same pair, same reason"),
    (
        "TIME",
        "answers with the clock, read once per surface a few hundred \
         microseconds apart. It was rejected from this list on the day \
         the register was written, correctly — the server did not carry \
         the verb then, so this harness would never have compared it. \
         Wiring it made the entry true",
    ),
];

/// The register: every verb on BOTH surfaces is either driven above with
/// real arguments, or named as one a byte comparison cannot settle.
///
/// `both_surfaces_refuse_a_short_call_the_same_way` already reaches every
/// shared verb — but only through its refusal. It proves the two surfaces
/// say no in the same sentence, and says nothing about what they answer
/// when they say yes. Nothing held that second face, and what the corpus
/// actually reached lived as a sentence in a roadmap, where it went stale:
/// it read "about a quarter" while the corpus had grown past half.
///
/// Held in BOTH directions, which is the point:
///
/// * a shared verb that is neither driven nor named fails — the register
///   cannot grow silently;
/// * a name here that the corpus has since started driving fails too — a
///   reason cannot outlive the thing it excused;
/// * a name here for a verb that is not on both surfaces fails — an entry
///   cannot be about a verb this harness was never going to compare.
///
/// And a floor, because a selector that returned nothing would satisfy
/// every one of those.
#[test]
fn every_shared_verb_is_driven_or_named() {
    use kevy_resp::ops_table::{spec, surface};
    use std::collections::BTreeSet;

    let shared: BTreeSet<&'static str> = kevy_resp::ops_table::ops_with(surface::SERVER)
        .into_iter()
        .filter(|n| spec(n).is_some_and(|s| s.surfaces & surface::ESTORE != 0))
        .collect();
    assert!(
        shared.len() > 50,
        "only {} verbs on both surfaces — the selector is broken, not the \
         engine bare",
        shared.len()
    );

    let driven: BTreeSet<&str> = CORPUS.iter().filter_map(|c| c.split(' ').next()).collect();
    let named: BTreeSet<&str> = CANNOT_COMPARE.iter().map(|(n, _)| *n).collect();

    let silent: Vec<&str> =
        shared.iter().copied().filter(|n| !driven.contains(n) && !named.contains(n)).collect();
    let healed: Vec<&str> = named.iter().copied().filter(|n| driven.contains(n)).collect();
    let stale: Vec<&str> = named.iter().copied().filter(|n| !shared.contains(n)).collect();

    println!(
        "differential register: {} of {} shared verbs driven, {} named as \
         beyond a byte comparison",
        shared.len() - silent.len() - named.len(),
        shared.len(),
        named.len()
    );

    assert!(
        silent.is_empty(),
        "{} verb(s) on both surfaces are neither driven by the corpus nor \
         named as beyond comparison — the differential is silent about \
         them: {silent:?}",
        silent.len()
    );
    assert!(
        healed.is_empty(),
        "{} verb(s) are named as beyond a byte comparison AND driven by \
         the corpus — one of the two is wrong: {healed:?}",
        healed.len()
    );
    assert!(
        stale.is_empty(),
        "{} verb(s) are named as beyond a byte comparison but are not on \
         both surfaces, so this harness would never have compared them: \
         {stale:?}",
        stale.len()
    );
}

/// TIME is named in `CANNOT_COMPARE`, so the corpus never drives it and
/// nothing else on the wire would either — which is how it reached CI
/// with fourteen never-executed regions after being wired.
///
/// A byte comparison against the facade is the thing that cannot be
/// made; asserting the SHAPE can. Two bulk strings of decimal digits,
/// the first a plausible unix second and the second inside one.
#[test]
fn time_answers_the_clock_in_redis_shape() {
    let server = Server::start_single_shard();
    let mut wire = server.wire();
    let reply = wire.call(&argv("TIME"));
    let text = String::from_utf8_lossy(&reply).to_string();
    let parts: Vec<&str> = text.split("\r\n").collect();
    assert_eq!(parts.first(), Some(&"*2"), "TIME did not answer a 2-element array: {text:?}");

    let secs: u64 = parts[2].parse().unwrap_or_else(|_| panic!("seconds not decimal: {text:?}"));
    let micros: u32 = parts[4].parse().unwrap_or_else(|_| panic!("micros not decimal: {text:?}"));
    assert!(micros < 1_000_000, "microseconds outside a second: {micros}");

    // A window wide enough that no clock skew or slow CI box trips it,
    // and narrow enough that a zero, a millisecond count or a
    // nanosecond count would not fit through: 2020-01-01 to 2100-01-01.
    assert!(
        (1_577_836_800..4_102_444_800).contains(&secs),
        "TIME's seconds are not a plausible unix second: {secs}"
    );
    // The declared lengths must match the digits, or a client reading
    // by length gets a truncated number.
    assert_eq!(parts[1], format!("${}", parts[2].len()));
    assert_eq!(parts[3], format!("${}", parts[4].len()));
}
