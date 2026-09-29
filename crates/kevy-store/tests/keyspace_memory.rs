//! What the keyspace table holds, counted at the allocator, against what
//! the store charges for it.
//!
//! Every key lives in one slot of an open-addressing table that doubles at
//! 7/8 load, so what a key costs is its slot divided by wherever the table
//! sits in that cycle, and the table holds its slots whether or not keys
//! fill them. A constant per key cannot say that: at the capacity
//! decomposition's scale the table held 124 bytes a key and was charged 96.
//!
//! Keys and values here are short enough to live inline, so the table is
//! the only allocation a write makes and `used_memory` has to move by
//! exactly what the allocator hands out for it — glibc's chunk for a table
//! small enough to be a heap block (16-byte granules, an 8-byte header).
//! The counters are per thread, so the harness's other threads do not leak
//! into a measurement.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use kevy_store::{SetCondition, Store};

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

/// Keys up to 7,000: the table doubles nine times and stays under the size
/// at which it is mapped directly instead of taken from the heap.
const KEYS: u64 = 7_000;

fn key(i: u64) -> Vec<u8> {
    format!("k:{i}").into_bytes()
}

type Check<'a> = &'a mut dyn FnMut(&Store, &'static str, u64);

/// Run `f` on a fresh store and check after every step that the charge is
/// what the allocator holds. Nothing the check itself does may allocate.
fn charged_is_held(what: &str, f: impl FnOnce(&mut Store, Check<'_>)) {
    let mut s = Store::new();
    let mut wrong: Vec<(&str, u64, i64, i64)> = Vec::with_capacity(8);
    LIVE.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let mut check = |s: &Store, step: &'static str, n: u64| {
        let held = LIVE.with(Cell::get);
        let charged = s.used_memory() as i64;
        if charged != held && wrong.len() < 8 {
            wrong.push((step, n, charged, held));
        }
    };
    f(&mut s, &mut check);
    COUNTING.with(|c| c.set(false));
    let lines: Vec<String> =
        wrong.iter().map(|(w, n, c, h)| format!("{w} {n}: charged {c}, held {h}")).collect();
    assert!(
        wrong.is_empty(),
        "{what}:
{}",
        lines.join(
            "
"
        )
    );
}

#[test]
fn the_keyspace_is_charged_the_table_it_holds() {
    charged_is_held("insert", |s, check| {
        for i in 0..KEYS {
            s.set(&key(i), b"v".to_vec(), None, SetCondition::Always);
            check(s, "after inserting key", i);
        }
    });
}

#[test]
fn a_deleted_key_leaves_its_slot_charged() {
    charged_is_held("delete", |s, check| {
        for i in 0..KEYS {
            s.set(&key(i), b"v".to_vec(), None, SetCondition::Always);
        }
        for i in 0..KEYS {
            s.del(&[key(i).as_slice()]);
            check(s, "after deleting key", i);
        }
        // the table is still there, and so is its charge
        assert!(s.used_memory() > 0);
    });
}

#[test]
fn a_flushed_keyspace_keeps_its_table_charged() {
    charged_is_held("flushall", |s, check| {
        for i in 0..KEYS {
            s.set(&key(i), b"v".to_vec(), None, SetCondition::Always);
        }
        s.flushall();
        check(s, "after FLUSHALL", 0);
        for i in 0..100 {
            s.set(&key(i), b"v".to_vec(), None, SetCondition::Always);
        }
        check(s, "after refilling", 0);
    });
}
