//! What a hash holds, counted at the allocator, against what the store
//! charges for it.
//!
//! Every allocation this thread makes passes through a counter that keeps
//! the bytes glibc would hand out for it (16-byte granules, an 8-byte
//! header, 32 at least) — the allocator a Linux deployment runs on, and
//! the one `used_memory` is modelled on. The counters are per thread, so
//! the harness's other threads do not leak into a measurement.
//!
//! The key is short enough to live inline and a string key is written
//! first, so the keyspace table is already allocated: what moves between
//! two readings is the hash and nothing else, and `used_memory` must move
//! by exactly that (plus the entry's fixed overhead when the key is new).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use kevy_store::{ENTRY_OVERHEAD, SetCondition, Store};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
}

fn chunk(n: usize) -> i64 {
    ((n + 8).next_multiple_of(16)).max(32) as i64
}

fn track(delta: i64) {
    if COUNTING.with(Cell::get) {
        LIVE.with(|c| c.set(c.get() + delta));
    }
}

struct Counting;

// SAFETY: every method forwards to `System`, which is a correct allocator;
// the counters are const-initialised thread locals without destructors, so
// touching them never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        track(chunk(l.size()));
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        track(chunk(l.size()));
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        track(-chunk(l.size()));
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        track(chunk(new) - chunk(l.size()));
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// A store whose keyspace table exists already.
fn store() -> Store {
    let mut s = Store::new();
    s.set(b"warm", b"1".to_vec(), None, SetCondition::Always);
    s
}

/// Run `f` and return (bytes the allocator still holds for it, change in
/// `used_memory`).
fn measure(s: &mut Store, f: impl FnOnce(&mut Store)) -> (i64, i64) {
    let used = s.used_memory() as i64;
    LIVE.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    f(s);
    COUNTING.with(|c| c.set(false));
    (LIVE.with(Cell::get), s.used_memory() as i64 - used)
}

fn field(i: usize, long: bool) -> Vec<u8> {
    if long {
        format!("field-with-a-long-name-{i:08}").into_bytes()
    } else {
        format!("f{i}").into_bytes()
    }
}

/// Charged minus held, for one hash built in one HSET.
fn one_hset(fields: usize, value_len: usize, long: bool) -> i64 {
    let mut s = store();
    let names: Vec<Vec<u8>> = (0..fields).map(|i| field(i, long)).collect();
    let value = vec![b'v'; value_len];
    let pairs: Vec<(&[u8], &[u8])> =
        names.iter().map(|n| (n.as_slice(), value.as_slice())).collect();
    let (held, charged) = measure(&mut s, |s| {
        s.hset(b"h", &pairs).expect("a hash");
    });
    charged - ENTRY_OVERHEAD as i64 - held
}

#[test]
fn a_hash_is_charged_what_it_holds() {
    let mut wrong = Vec::new();
    for fields in [1, 2, 3, 5, 8, 10, 14, 15, 20, 50, 100, 1000] {
        for value_len in [8, 22, 23, 24, 64, 900, 4096] {
            for long in [false, true] {
                let off = one_hset(fields, value_len, long);
                if off != 0 {
                    wrong.push(format!(
                        "{fields} fields x {value_len} B (long names {long}): {off:+}"
                    ));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "charged minus held:\n{}", wrong.join("\n"));
}

/// The capacity decomposition's row: four short fields and a 900-byte pad.
/// glibc holds 1808 bytes for it — an 80-byte map box, an 816-byte table
/// of sixteen slots, the pad's 912.
#[test]
fn the_decomposition_row_weighs_1808() {
    let mut s = store();
    let pad = vec![b'p'; 900];
    let pairs: [(&[u8], &[u8]); 5] = [
        (b"id", b"123"),
        (b"status", b"active"),
        (b"score", b"42"),
        (b"ts", b"1700000000"),
        (b"pad", &pad),
    ];
    let (held, charged) = measure(&mut s, |s| {
        s.hset(b"row:00000001", &pairs).expect("a hash");
    });
    assert_eq!(held, 1808);
    assert_eq!(charged - ENTRY_OVERHEAD as i64, 1808);
}

/// Field by field, up and back down, through every representation change:
/// inline to table, each table growth, and (past the sharding threshold)
/// table to shards — checked after every single write.
#[test]
fn every_write_moves_the_charge_by_what_it_moved_on_the_heap() {
    for value_len in [4, 30, 300] {
        let mut s = store();
        let value = vec![b'v'; value_len];
        let mut drift = 0i64;
        let mut check = |s: &mut Store, what: &str, f: &mut dyn FnMut(&mut Store)| {
            let (held, charged) = measure(s, |s| f(s));
            // the first write also creates the entry
            let fixed = if what == "create" { ENTRY_OVERHEAD as i64 } else { 0 };
            drift += charged - fixed - held;
            assert_eq!(drift, 0, "{what}, {value_len}-byte values: charged {charged}, held {held}");
        };
        check(&mut s, "create", &mut |s| {
            s.hset(b"h", &[(b"f0".as_slice(), value.as_slice())]).expect("a hash");
        });
        for i in 1..200 {
            let f = field(i, i % 3 == 0);
            check(&mut s, "hset", &mut |s| {
                s.hset(b"h", &[(f.as_slice(), value.as_slice())]).expect("a hash");
            });
        }
        for i in 0..50 {
            let f = field(i, i % 3 == 0);
            check(&mut s, "overwrite", &mut |s| {
                s.hset(b"h", &[(f.as_slice(), b"x".as_slice())]).expect("a hash");
            });
        }
        for i in 200..230 {
            let f = field(i, false);
            check(&mut s, "hincrby", &mut |s| {
                s.hincrby(b"h", &f, 7).expect("an integer field");
            });
        }
        for i in 0..199 {
            let f = field(i, i % 3 == 0);
            check(&mut s, "hdel", &mut |s| {
                s.hdel(b"h", &[f.as_slice()]).expect("a hash");
            });
        }
    }
}

/// Past the sharding threshold the hash is a directory of shards; its
/// charge has to follow the shards, not a flat table's model.
#[test]
fn a_sharded_hash_is_charged_what_it_holds() {
    for fields in [20_000, 40_000] {
        for value_len in [8, 64] {
            assert_eq!(one_hset(fields, value_len, false), 0, "{fields} fields x {value_len} B");
        }
    }
}
