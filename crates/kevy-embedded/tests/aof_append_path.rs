//! What logging a write to the AOF costs the caller's thread, counted
//! rather than timed, and what it leaves on disk, compared byte for byte.
//!
//! The crate forbids unsafe code, so the counting allocator lives here,
//! in the test crate, where installing one is allowed. Counting is per
//! thread: the background reaper's allocations are not the caller's.

#![cfg(feature = "persist")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use kevy_embedded::{AppendFsync, Config, Store};

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

fn note(size: usize) {
    // try_with: the allocator can run while this thread's locals are torn down
    let _ = ON.try_with(|on| {
        if on.get() {
            ALLOCS.with(|n| n.set(n.get() + 1));
            BYTES.with(|b| b.set(b.get() + size as u64));
        }
    });
}

struct Counting;

// SAFETY: every method forwards to `System`, which is a correct allocator;
// the counters are thread-local cells and touch no allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note(l.size());
        // SAFETY: `l` came from the caller and is forwarded unchanged.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p`/`l` came from the caller and are forwarded unchanged.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        note(new);
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocation calls and bytes requested on this thread while `f` runs.
fn count<F: FnOnce()>(f: F) -> (u64, u64) {
    ALLOCS.with(|n| n.set(0));
    BYTES.with(|b| b.set(0));
    ON.with(|o| o.set(true));
    f();
    ON.with(|o| o.set(false));
    (ALLOCS.with(Cell::get), BYTES.with(Cell::get))
}

/// Per-SET allocations after warm-up, and the first SET of a 1 MiB value.
fn set_costs(store: &Store) -> ((u64, u64), (u64, u64)) {
    let big = vec![9u8; 1 << 20];
    let first_big = count(|| {
        store.set(b"big", &big).expect("set big");
    });
    let v = vec![7u8; 4096];
    for _ in 0..200 {
        store.set(b"k", &v).expect("warm-up set");
    }
    let steady = count(|| {
        for _ in 0..100 {
            store.set(b"k", &v).expect("set");
        }
    });
    (steady, first_big)
}

/// Logging a SET to the AOF allocates nothing on the caller's thread: a
/// persistent store pays exactly what a memory-only store pays for the
/// same writes, and no intermediate buffer grows to hold even a 1 MiB
/// frame. Before the log encoded from the caller's slices, the same
/// 100 SETs of 4 KiB cost 600 more allocations and about 800 KiB more,
/// and the 1 MiB value cost another 5 MiB.
#[test]
fn logging_a_set_allocates_nothing_on_the_callers_thread() {
    let mem = Store::open(Config::default().with_shards(1)).expect("open memory store");
    let (mem_steady, mem_big) = set_costs(&mem);

    let dir = kevy_tmpdir::TmpDir::new("aof-append-allocs");
    let cfg = Config::default()
        .with_shards(1)
        .with_persist(dir.path())
        .with_appendfsync(AppendFsync::EverySec)
        .with_auto_aof_rewrite_disabled();
    let disk = Store::open(cfg).expect("open persistent store");
    let (disk_steady, disk_big) = set_costs(&disk);

    assert!(mem_steady.0 > 0, "the counter must see the store's own value copy");
    assert_eq!(disk_steady, mem_steady, "100 SETs of 4 KiB: (allocations, bytes)");
    assert_eq!(disk_big, mem_big, "first SET of 1 MiB: (allocations, bytes)");
}

/// Allocations for 100 `XADD`s through the argv path, explicit IDs then
/// generated ones of the same length (the reply formats both the same way).
#[cfg(feature = "streams-geo")]
fn xadd_costs(store: &Store) -> (u64, u64) {
    let argv = |id: &str| -> Vec<Vec<u8>> {
        ["XADD", "s", id, "f", "v"].iter().map(|p| p.as_bytes().to_vec()).collect()
    };
    let explicit: Vec<_> =
        (0..200).map(|i| argv(&format!("{}-0", 1_000_000_000_000u64 + i))).collect();
    let star = argv("*");
    let mut out = Vec::with_capacity(4096);
    let mut run = |a: &Vec<Vec<u8>>| {
        out.clear();
        store.dispatch_argv(a, &mut out);
        assert_eq!(out.first(), Some(&b'$'), "{:?}", String::from_utf8_lossy(&out));
    };
    explicit[..100].iter().for_each(&mut run);
    let with_id = count(|| explicit[100..].iter().for_each(&mut run)).0;
    (0..100).for_each(|_| run(&star));
    let with_star = count(|| (0..100).for_each(|_| run(&star))).0;
    (with_id, with_star)
}

/// A generated `XADD` ID is recorded as the ID it gave without a frame
/// built for it: the record is the argv viewed from the stack with the
/// ID swapped in, so it costs what an explicit ID does, logged or not.
#[cfg(feature = "streams-geo")]
#[test]
fn a_generated_stream_id_is_recorded_for_what_an_explicit_one_costs() {
    let mem = Store::open(Config::default().with_shards(1)).expect("open memory store");
    let (mem_id, mem_star) = xadd_costs(&mem);
    let dir = kevy_tmpdir::TmpDir::new("aof-append-xadd");
    let cfg = Config::default()
        .with_shards(1)
        .with_persist(dir.path())
        .with_appendfsync(AppendFsync::EverySec)
        .with_auto_aof_rewrite_disabled();
    let (disk_id, disk_star) = xadd_costs(&Store::open(cfg).expect("open persistent store"));
    assert!(mem_id >= 100, "the counter must see the entries: {mem_id}");
    assert_eq!((mem_star, disk_star), (mem_id, disk_id), "100 XADDs: generated vs explicit");
}

/// The log an embedded store writes is the canonical record stream, byte
/// for byte: magic, then one checksummed multibulk record per write.
#[test]
fn the_log_is_the_canonical_record_stream() {
    let dir = kevy_tmpdir::TmpDir::new("aof-append-bytes");
    let cfg =
        Config::default().with_shards(1).with_persist(dir.path()).with_auto_aof_rewrite_disabled();
    let big = vec![0x5Au8; 300_000];
    {
        let store = Store::open(cfg).expect("open");
        store.set(b"k", b"v").expect("set");
        store.set(b"e", b"").expect("set empty");
        store.set(b"big", &big).expect("set big");
        store.del(&[b"e".as_slice()]).expect("del");
    }
    let mut want = kevy_persist::AOF2_MAGIC.to_vec();
    let mut scratch = Vec::new();
    let writes: [&[&[u8]]; 4] =
        [&[b"SET", b"k", b"v"], &[b"SET", b"e", b""], &[b"SET", b"big", &big], &[b"DEL", b"e"]];
    for w in writes {
        let argv = kevy_persist::Argv::from(w.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
        kevy_persist::write_record_multibulk(&mut want, &argv, &mut scratch).expect("encode");
    }
    let got = std::fs::read(dir.path().join("aof-0.aof")).expect("read log");
    assert_eq!(got.len(), want.len(), "log length");
    assert!(got == want, "log bytes differ from the canonical record stream");
}
