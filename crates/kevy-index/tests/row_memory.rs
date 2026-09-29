//! What one index row costs on the heap, counted rather than reasoned
//! about, and whether `SegmentStats::approx_bytes` agrees with the count.
//!
//! The counters are per thread, so the other tests in this binary (which
//! the harness runs on their own threads) do not leak into a measurement.
//! Alongside the requested bytes the test prints what glibc would hand
//! out for the same requests (16-byte granules, an 8-byte header, 32 at
//! least), since that is what a Linux deployment pays; only the requested
//! bytes are asserted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::mem::size_of;

use kevy_index::{CompositeCol, IndexValue, Segment, ValType, composite_encode};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static LIVE_BYTES: Cell<isize> = const { Cell::new(0) };
    static LIVE_GLIBC: Cell<isize> = const { Cell::new(0) };
    static LIVE_ALLOCS: Cell<isize> = const { Cell::new(0) };
}

fn glibc_chunk(n: usize) -> isize {
    ((n + 8).next_multiple_of(16)).max(32) as isize
}

fn track(bytes: isize, glibc: isize, allocs: isize) {
    if COUNTING.with(Cell::get) {
        LIVE_BYTES.with(|c| c.set(c.get() + bytes));
        LIVE_GLIBC.with(|c| c.set(c.get() + glibc));
        LIVE_ALLOCS.with(|c| c.set(c.get() + allocs));
    }
}

struct Counting;

// SAFETY: every method forwards to `System`, which is a correct
// allocator; the counters are const-initialised thread locals without
// destructors, so touching them never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        track(l.size() as isize, glibc_chunk(l.size()), 1);
        // SAFETY: `l` came from the caller and is forwarded unchanged.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        track(-(l.size() as isize), -glibc_chunk(l.size()), -1);
        // SAFETY: `p`/`l` came from the caller and are forwarded unchanged.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        track(new as isize - l.size() as isize, glibc_chunk(new) - glibc_chunk(l.size()), 0);
        // SAFETY: forwarded unchanged.
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

struct Rows {
    keys: Vec<Vec<u8>>,
    vals: Vec<IndexValue>,
}

impl Rows {
    fn avg_key(&self) -> f64 {
        self.keys.iter().map(Vec::len).sum::<usize>() as f64 / self.keys.len() as f64
    }

    /// Heap bytes a value owns (0 for the numeric variants).
    fn avg_value_heap(&self) -> f64 {
        let heap = |v: &IndexValue| match v {
            IndexValue::Str(s) => s.len(),
            _ => 0,
        };
        self.vals.iter().map(heap).sum::<usize>() as f64 / self.vals.len() as f64
    }
}

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 33
}

/// `row:N` keys (or a 36-byte-prefixed long form), an i64 value shared
/// by about twenty rows, inserted in random value order.
fn scalar_rows(n: usize, long: bool) -> Rows {
    let mut s = 42u64;
    let skus = (n / 20).max(1) as u64;
    let key = |i| match long {
        true => format!("row:tenant-acme-production-cluster-{i}").into_bytes(),
        false => format!("row:{i}").into_bytes(),
    };
    let keys = (0..n).map(key).collect();
    let vals = (0..n).map(|_| IndexValue::I64((lcg(&mut s) % skus) as i64)).collect();
    Rows { keys, vals }
}

/// An ORDERPATH-shaped value: eight departments, then a timestamp that
/// grows with every row, so nearly every value is distinct.
fn composite_rows(n: usize) -> Rows {
    const DEPTS: [&str; 8] = ["eng", "ops", "sales", "hr", "legal", "design", "data", "support"];
    let cols = [CompositeCol::new("dept", ValType::Str), CompositeCol::new("ts", ValType::I64)];
    let keys = (0..n).map(|i| format!("row:{i}").into_bytes()).collect();
    let vals = (0..n)
        .map(|i| {
            let ts = (1_700_000_000 + i).to_string();
            let dept = DEPTS[i % 8].as_bytes();
            let enc = composite_encode(&cols, &[Some(dept), Some(ts.as_bytes())]);
            IndexValue::Str(enc.expect("both columns coerce"))
        })
        .collect();
    Rows { keys, vals }
}

struct Held {
    bytes: f64,
    glibc: f64,
    allocs: f64,
    approx: f64,
}

/// Build a segment from `rows` and report, per row, the heap it still
/// holds, the allocations that heap is split into, and its own estimate.
fn measure(rows: &Rows) -> Held {
    measure_with(rows, &[])
}

/// [`measure`] for an index that also stores `stored` for every row.
fn measure_with(rows: &Rows, stored: &[&[u8]]) -> Held {
    let n = rows.keys.len() as f64;
    let vals: Vec<Option<&[u8]>> = stored.iter().map(|v| Some(*v)).collect();
    LIVE_BYTES.with(|c| c.set(0));
    LIVE_GLIBC.with(|c| c.set(0));
    LIVE_ALLOCS.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let mut seg =
        if stored.is_empty() { Segment::new() } else { Segment::with_values(stored.len()) };
    for (k, v) in rows.keys.iter().zip(&rows.vals) {
        seg.apply_with_values(k, Some(v.clone()), &vals);
    }
    COUNTING.with(|c| c.set(false));
    let held = Held {
        bytes: LIVE_BYTES.with(Cell::get) as f64 / n,
        glibc: LIVE_GLIBC.with(Cell::get) as f64 / n,
        allocs: LIVE_ALLOCS.with(Cell::get) as f64 / n,
        approx: seg.stats().approx_bytes as f64 / n,
    };
    drop(seg);
    held
}

/// The ceiling one row may cost, from the layout rather than a fitted
/// constant: one allocation holding a 32-bit count, the key's 32-bit
/// length, the value and the key's bytes, padded to 8; any string value's
/// bytes; one pointer slot in the ordered tree; and one pointer slot plus a
/// control byte in the reverse set.
fn ceiling(rows: &Rows) -> f64 {
    let entry = (2 * size_of::<u32>() + size_of::<IndexValue>() + 7) as f64;
    let ptr = size_of::<usize>() as f64;
    // every B-tree node but the root fills at least 5 of its 11 slots; a
    // leaf is the slots behind a 16-byte header, an internal node adds 12
    // edges, and a tree with L leaves keeps only L - 1 elements in
    // internal nodes, so at most one element in six lives there
    let leaf = 11.0 * ptr + 16.0;
    let internal = leaf + 12.0 * ptr;
    let tree = 5.0 / 6.0 * leaf / 5.0 + 1.0 / 6.0 * internal / 5.0;
    // the reverse set keeps between 1.14 and 2.29 slots per row
    let back = (ptr + 1.0) * 2.29;
    entry + rows.avg_key() + rows.avg_value_heap() + tree + back
}

// std's hash table resizes at 7/8 of a power-of-two bucket count: with
// 2^16 buckets it holds 57 344 rows at 1.14 slots per row, and one more
// row doubles it to 2.29 — both ends of the saw-tooth
const FULL: usize = 57_344;
const JUST_GROWN: usize = FULL + 1;

fn check(label: &str, rows: &Rows, max_allocs: f64) {
    let h = measure(rows);
    let cap = ceiling(rows);
    println!(
        "{label}: n={} held {:.1} B/row (glibc {:.1}) in {:.2} allocs/row, approx_bytes {:.1} B/row, ceiling {:.1}",
        rows.keys.len(),
        h.bytes,
        h.glibc,
        h.allocs,
        h.approx,
        cap
    );
    assert!(h.allocs <= max_allocs, "{label}: {:.2} allocs/row > {max_allocs}", h.allocs);
    assert!(h.bytes <= cap, "{label}: {:.1} B/row > structural ceiling {cap:.1}", h.bytes);
    let off = (h.approx - h.bytes).abs() / h.bytes;
    assert!(off <= 0.10, "{label}: approx_bytes {:.1} vs held {:.1} ({off:.3})", h.approx, h.bytes);
}

#[test]
fn a_scalar_row_is_one_entry_and_one_key() {
    for n in [FULL, JUST_GROWN] {
        // the row, and the tree's and table's share of their nodes
        check("i64, short key", &scalar_rows(n, false), 1.2);
        check("i64, long key", &scalar_rows(n, true), 1.2);
    }
}

#[test]
fn a_composite_row_holds_its_value_once() {
    for n in [FULL, JUST_GROWN] {
        check("composite", &composite_rows(n), 2.2);
    }
}

/// The stored-value column is a hash table of its own, keyed by row. Its
/// slots are as much the index's memory as the rows are.
#[test]
fn a_values_row_counts_its_side_table() {
    for n in [FULL, JUST_GROWN] {
        let rows = scalar_rows(n, false);
        for stored in [&[&b"active"[..], b"1700000000"][..], &[b"eng"]] {
            let h = measure_with(&rows, stored);
            println!(
                "values x{}: n={n} held {:.1} B/row (glibc {:.1}), approx_bytes {:.1} B/row",
                stored.len(),
                h.bytes,
                h.glibc,
                h.approx
            );
            let off = (h.approx - h.bytes).abs() / h.bytes;
            assert!(off <= 0.10, "approx_bytes {:.1} vs held {:.1} ({off:.3})", h.approx, h.bytes);
        }
    }
}
