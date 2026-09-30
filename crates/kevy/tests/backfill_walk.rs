//! A declaration's backfill walks the keyspace; it does not copy it.
//!
//! `TABLE.DECLARE` starts three backfills on every shard — one per
//! compiled index and one that packs the existing rows — and each used to
//! begin by copying every key under the table's prefix into a list. Ten
//! million keys made that 2.17 GB held at once, and the freed copies left
//! about a gigabyte of small chunks the allocator could not return. The
//! walk now carries a cursor, so what a backfill holds is one batch.
//!
//! Everything runs on the test thread against an in-process command set,
//! so a per-thread allocation counter sees exactly the declaration's
//! bytes. The second half is the price of not having a snapshot: rows
//! written, deleted and moved while the walk is under way must end up
//! indexed exactly as a fresh build over the final keyspace would.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;

use kevy::{Argv, KevyCommands, KeyspaceStore};
use kevy_resp::RespVersion;
use kevy_rt::{Commands, ExtensionReduced};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

fn track(delta: i64) {
    if COUNTING.with(Cell::get) {
        let live = LIVE.with(|c| {
            c.set(c.get() + delta);
            c.get()
        });
        PEAK.with(|p| p.set(p.get().max(live)));
    }
}

struct Counting;

// SAFETY: every method forwards to `System`, which is a correct allocator;
// the counters are const-initialised thread locals without destructors, so
// touching them never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        track(l.size() as i64);
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        track(l.size() as i64);
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        track(-(l.size() as i64));
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        track(new as i64 - l.size() as i64);
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The table's columns; every row this file writes fits them.
const COLUMNS: &str = "TABLE.DECLARE env PREFIX row: PK id COLUMN id i64 COLUMN status str \
                       COLUMN score i64 COLUMN ts i64";
/// The same table with a VALUES index and an ORDERPATH compiled from it.
const INDEXES: &str = "INDEX score range VALUES status ts \
                       ORDERPATH by_status_ts ON status THEN ts DESC";

const STATUSES: [&str; 4] = ["new", "paid", "sent", "void"];

struct Db {
    kevy: KevyCommands,
    store: KeyspaceStore,
    /// Give every row a field the table does not declare, so no row can
    /// take the packed form.
    note: bool,
}

impl Db {
    fn new() -> Self {
        Db { kevy: KevyCommands::new(), store: KeyspaceStore::new(), note: false }
    }

    fn declare(&mut self, with_indexes: bool) {
        let text = if with_indexes { format!("{COLUMNS} {INDEXES}") } else { COLUMNS.into() };
        let parts: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(self.call(&parts), b"+OK\r\n");
    }

    fn call(&mut self, parts: &[&str]) -> Vec<u8> {
        let argv = Argv::from(parts.iter().map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>());
        self.kevy.dispatch(&mut self.store, &argv)
    }

    /// A fanned-out read as the runtime serves it on a one-shard server:
    /// each phase runs on the shard, then reduces.
    fn query(&mut self, parts: &[&str]) -> Vec<u8> {
        let mut argv: Vec<Vec<u8>> = parts.iter().map(|p| p.as_bytes().to_vec()).collect();
        loop {
            let chunk = self.kevy.extension_op(&mut self.store, &argv);
            match self.kevy.extension_reduce(&argv, vec![chunk], RespVersion::V2) {
                ExtensionReduced::Continue(next) => argv = next,
                ExtensionReduced::Reply(reply) => return reply,
                _ => unreachable!("a reduce answers or continues"),
            }
        }
    }

    /// A write as the runtime performs it: the command, then the hook.
    fn write(&mut self, parts: &[&str], keys: &[&str]) {
        let reply = self.call(parts);
        assert!(!reply.starts_with(b"-"), "{parts:?}: {}", String::from_utf8_lossy(&reply));
        for k in keys {
            self.kevy.on_write(&mut self.store, k.as_bytes());
        }
    }

    fn put_row(&mut self, i: u64, score: i64) {
        let key = format!("row:{i}");
        let (id, score) = (i.to_string(), score.to_string());
        let ts = (1_700_000_000 + i).to_string();
        let status = STATUSES[(i % 4) as usize];
        let mut argv = vec!["HSET", &key, "id", &id, "status", status, "score", &score, "ts", &ts];
        if self.note {
            argv.extend(["note", "an undeclared field"]);
        }
        self.write(&argv, &[&key]);
    }

    fn building(&mut self) -> bool {
        let score = self.query(&["IDX.QUERY", "env.score", "EQ", "0"]);
        let path = self.query(&["IDX.QUERY", "env.by_status_ts", "WHERE", "status", "EQ", "x"]);
        score.starts_with(b"-INDEXBUILDING") || path.starts_with(b"-INDEXBUILDING")
    }

    /// What the table's indexes report they hold, summed (`IDX.LIST`'s
    /// `bytes`).
    fn index_bytes(&mut self) -> i64 {
        let reply = String::from_utf8(self.query(&["IDX.LIST"])).expect("utf-8");
        let mut lines = reply.split("\r\n").filter(|l| !l.starts_with(['*', '$']));
        let mut total = 0;
        while let Some(l) = lines.next() {
            if l == "bytes" {
                let v = lines.next().expect("a value after bytes");
                total += v.trim_start_matches(':').parse::<i64>().expect("a byte count");
            }
        }
        total
    }

    /// Every key the score index answers, with the score it holds, read
    /// a page at a time.
    fn score_entries(&mut self) -> BTreeMap<String, i64> {
        let mut out = BTreeMap::new();
        let mut cursor = "0".to_string();
        loop {
            let reply = self.query(&[
                "IDX.QUERY",
                "env.score",
                "RANGE",
                "-1000000000",
                "1000000000",
                "LIMIT",
                "10000",
                "CURSOR",
                &cursor,
            ]);
            // [cursor, key, value, key, value, …]
            let items = bulks(&reply);
            for kv in items[1..].chunks(2) {
                out.insert(kv[0].clone(), kv[1].parse().expect("an integer score"));
            }
            cursor.clone_from(&items[0]);
            if cursor == "0" {
                return out;
            }
        }
    }
}

/// The bulk strings of a flat RESP reply, in order.
fn bulks(reply: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(reply);
    let mut lines = text.split("\r\n").peekable();
    let mut out = Vec::new();
    while let Some(line) = lines.next() {
        if line.starts_with('$') && line != "$-1" {
            out.push(lines.next().unwrap_or_default().to_string());
        } else if let Some(n) = line.strip_prefix(':') {
            out.push(n.to_string());
        }
    }
    out
}

fn load(db: &mut Db, rows: u64) {
    for i in 0..rows {
        db.put_row(i, (i % 1000) as i64);
    }
}

/// Bytes above the starting point, at most and at the end, while `f` runs.
fn held_while(f: impl FnOnce()) -> (i64, i64) {
    LIVE.with(|c| c.set(0));
    PEAK.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    (PEAK.with(Cell::get), LIVE.with(Cell::get))
}

/// Tick until both compiled paths serve.
fn build(db: &mut Db) -> usize {
    let mut ticks = 0;
    while db.building() {
        db.kevy.on_shard_tick(&mut db.store);
        ticks += 1;
        assert!(ticks < 100_000, "the backfill never finished");
    }
    ticks
}

// One copy of every key is about 34 bytes a row here: a nine-byte key and
// its 24-byte handle in the list. A walk holds one batch of them.
const ROWS: u64 = 100_000;
const HELD_PER_ROW: i64 = 8;
// The walk visits rows in hash order, so an index's leaves fill to about
// ln 2 before the final repack packs them: the tree peaks at up to 1/0.69
// of what it keeps
const FILL_SLACK: f64 = 1.0 / 0.69 - 1.0;

#[test]
fn an_index_backfill_holds_one_batch_of_keys() {
    let mut db = Db::new();
    db.note = true;
    load(&mut db, ROWS);
    let started = std::time::Instant::now();
    let mut ticks = 0;
    let (peak, kept) = held_while(|| {
        db.declare(true);
        ticks = build(&mut db);
    });
    let took = started.elapsed();
    // anything held above what the build keeps at the end was transient:
    // the unpacked leaves, and whatever the walk held
    let excess = peak - kept;
    let slack = (db.index_bytes() as f64 * FILL_SLACK) as i64;
    let walk = excess - slack;
    eprintln!(
        "index backfill: {ROWS} rows, {ticks} ticks, {took:?}; peak {peak} B, kept {kept} B, \
         transient {excess} B = {:.1} B/row, of which leaf slack at most {slack} B",
        excess as f64 / ROWS as f64
    );
    assert_eq!(db.score_entries().len(), ROWS as usize, "every row is indexed");
    assert!(!db.store.is_packed(b"row:0"), "a row with an undeclared field is never packed");
    assert!(
        walk < ROWS as i64 * HELD_PER_ROW,
        "the build held {excess} bytes above what it keeps, {walk} beyond the leaves' slack \
         ({:.1} B/row)",
        walk as f64 / ROWS as f64
    );
}

#[test]
fn a_packing_backfill_holds_one_batch_of_keys() {
    let mut db = Db::new();
    load(&mut db, ROWS);
    // a batch of 2048 rows a tick, and one tick more for a walk's overshoot
    let ticks = ROWS / 2048 + 2;
    let started = std::time::Instant::now();
    let (peak, kept) = held_while(|| {
        db.declare(false);
        for _ in 0..ticks {
            db.kevy.on_shard_tick(&mut db.store);
        }
    });
    let took = started.elapsed();
    let unpacked = (0..ROWS).filter(|i| !db.store.is_packed(format!("row:{i}").as_bytes()));
    assert_eq!(unpacked.count(), 0, "every row is packed");
    eprintln!(
        "packing backfill: {ROWS} rows, {ticks} ticks, {took:?}; peak {peak} B above the \
         start, {kept} B at the end = {:.1} B/row",
        kept as f64 / ROWS as f64
    );
    assert!(kept < 0, "packing gives memory back");
    // packing only frees, so the peak is whatever the backfill held on top
    assert!(
        peak < ROWS as i64 * HELD_PER_ROW,
        "the packing walk held {peak} bytes ({:.1} B/row)",
        peak as f64 / ROWS as f64
    );
}

#[test]
fn rows_written_deleted_and_moved_during_the_walk_are_indexed_as_they_end() {
    const ROWS: u64 = 20_000;
    let mut db = Db::new();
    load(&mut db, ROWS);
    let mut want: BTreeMap<String, i64> =
        (0..ROWS).map(|i| (format!("row:{i}"), (i % 1000) as i64)).collect();
    db.declare(true);
    let mut round = 0u64;
    let mut next = ROWS;
    while db.building() {
        db.kevy.on_shard_tick(&mut db.store);
        // rewrite, delete, create and rename some rows, spread over the
        // keyspace so both walked and unwalked keys are hit
        for j in 0..50 {
            let i = (round * 7919 + j * 104_729) % ROWS;
            let key = format!("row:{i}");
            match j % 5 {
                0 | 1 if want.contains_key(&key) => {
                    let score = 5000 + (round * 50 + j) as i64;
                    db.put_row(i, score);
                    want.insert(key, score);
                }
                2 => {
                    db.write(&["DEL", &key], &[&key]);
                    want.remove(&key);
                }
                3 => {
                    db.put_row(next, -1);
                    want.insert(format!("row:{next}"), -1);
                    next += 1;
                }
                _ if want.contains_key(&key) => {
                    let to = format!("row:{next}");
                    next += 1;
                    db.write(&["RENAME", &key, &to], &[&key, &to]);
                    let score = want.remove(&key).expect("present");
                    want.insert(to, score);
                }
                _ => {}
            }
        }
        round += 1;
        assert!(round < 10_000, "the backfill never finished");
    }
    assert!(round > 1, "the writes have to land while the walk is under way");
    let got = db.score_entries();
    let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).take(5).collect();
    let stale: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).take(5).collect();
    let wrong: Vec<_> =
        want.iter().filter(|(k, v)| got.get(*k).is_some_and(|g| g != *v)).take(5).collect();
    assert!(
        missing.is_empty() && stale.is_empty() && wrong.is_empty(),
        "missing {missing:?}, stale {stale:?}, wrong value {wrong:?}"
    );
    assert_eq!(got.len(), want.len());
}
