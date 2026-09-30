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

use kevy_index::{
    CompositeCol, IndexKind, IndexSpec, IndexValue, Segment, SortOrder, ValType, ValueSpec,
    composite_encode,
};

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

/// One row: key, value, stored values.
type Row = (Vec<u8>, IndexValue, Vec<Vec<u8>>);

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 33
}

fn spec(ty: ValType, composite: bool, values: usize) -> IndexSpec {
    let mut b = IndexSpec::builder("t.i", "row:", IndexKind::Range, ty).with_field("f");
    if composite {
        b = b.with_composite(vec![
            CompositeCol::new("dept", ValType::Str),
            CompositeCol::new("ts", ValType::I64).with_order(SortOrder::Desc),
        ]);
    }
    if values > 0 {
        b = b.with_values((0..values).map(|i| ValueSpec::new(format!("v{i}"))).collect());
    }
    b.build().expect("a spec")
}

/// `row:<i>` keys (ids stride 8, as one shard of eight sees them) with a
/// random i64 value, inserted in id order: random value order.
fn scalar_rows(n: usize, values: usize) -> Vec<Row> {
    let mut s = 42u64;
    (0..n)
        .map(|i| {
            let stored = (0..values)
                .map(|v| {
                    if v == 0 {
                        b"s1".to_vec()
                    } else {
                        format!("{}", 1_700_000_000 + i).into_bytes()
                    }
                })
                .collect();
            (format!("row:{}", i * 8).into_bytes(), IndexValue::I64(lcg(&mut s) as i64), stored)
        })
        .collect()
}

/// An ORDERPATH-shaped value: five statuses, then a timestamp that grows
/// with every row, newest first.
fn composite_rows(n: usize) -> Vec<Row> {
    let cols = [
        CompositeCol::new("dept", ValType::Str),
        CompositeCol::new("ts", ValType::I64).with_order(SortOrder::Desc),
    ];
    (0..n)
        .map(|i| {
            let ts = (1_700_000_000 + i).to_string();
            let dept = format!("s{}", i % 5);
            let enc = composite_encode(&cols, &[Some(dept.as_bytes()), Some(ts.as_bytes())]);
            (
                format!("row:{}", i * 8).into_bytes(),
                IndexValue::Str(enc.expect("coerces")),
                Vec::new(),
            )
        })
        .collect()
}

struct Held {
    bytes: f64,
    glibc: f64,
    allocs: f64,
    approx: f64,
}

/// Build a segment for `spec` from `rows` (then repack when `packed`) and
/// report, per row, the heap it holds and its own estimate.
fn measure(spec: &IndexSpec, rows: &[Row], packed: bool) -> Held {
    let n = rows.len() as f64;
    LIVE_BYTES.with(|c| c.set(0));
    LIVE_GLIBC.with(|c| c.set(0));
    LIVE_ALLOCS.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let mut seg = Segment::for_spec(spec);
    for (k, v, stored) in rows {
        let vals: Vec<Option<&[u8]>> = stored.iter().map(|s| Some(s.as_slice())).collect();
        seg.apply_with_values(k, None, Some(v.clone()), &vals);
    }
    if packed {
        seg.repack();
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

/// The ceiling for a row whose packed entry takes `entry` bytes of a leaf
/// (slot, lengths, the order key past its first 8 bytes, the payload):
/// every leaf but the first and last keeps two-thirds of its 1768-byte
/// page less two entries whatever order rows arrive in, and is full after
/// a repack less one entry; the inner levels and the leaf header add
/// under 4%.
fn ceiling(entry: f64, packed: bool) -> f64 {
    let fill = if packed { 0.95 } else { ((1768 * 2 / 3) as f64 - 2.0 * entry) / 1768.0 };
    entry / fill * 1.04
}

fn check(label: &str, spec: &IndexSpec, rows: &[Row], entry: f64) {
    for packed in [false, true] {
        let h = measure(spec, rows, packed);
        let cap = ceiling(entry, packed);
        println!(
            "{label} ({}): n={} held {:.1} B/row (glibc {:.1}) in {:.3} allocs/row, approx_bytes {:.1} B/row, ceiling {:.1}",
            if packed { "repacked" } else { "random order" },
            rows.len(),
            h.bytes,
            h.glibc,
            h.allocs,
            h.approx,
            cap
        );
        assert!(
            h.allocs <= 0.05,
            "{label}: {:.3} allocs/row: rows are not allocated one by one",
            h.allocs
        );
        assert!(h.bytes <= cap, "{label}: {:.1} B/row > layout ceiling {cap:.1}", h.bytes);
        let off = (h.approx - h.bytes).abs() / h.bytes;
        assert!(
            off <= 0.10,
            "{label}: approx_bytes {:.1} vs held {:.1} ({off:.3})",
            h.approx,
            h.bytes
        );
    }
}

const N: usize = 100_000;

#[test]
fn a_scalar_row_is_its_value_and_its_packed_id() {
    // slot 10, key length 1, id digits (up to 6 here) packed into 3
    check("i64", &spec(ValType::I64, false, 0), &scalar_rows(N, 0), 10.0 + 1.0 + 3.0);
}

#[test]
fn a_composite_row_holds_its_encoding_once() {
    // slot 10, key and value lengths 2, the encoding past 8 bytes (4 + 8
    // - 8) and the packed id (3)
    check("composite", &spec(ValType::Str, true, 0), &composite_rows(N), 10.0 + 2.0 + 4.0 + 3.0);
}

#[test]
fn a_values_row_carries_its_values_in_the_leaf() {
    // slot 10, key and payload lengths 2, the id (3), `s1` (1 + 2) and a
    // ten-digit number packed (1 + 5)
    check(
        "values x2",
        &spec(ValType::I64, false, 2),
        &scalar_rows(N, 2),
        10.0 + 2.0 + 3.0 + 3.0 + 6.0,
    );
}
