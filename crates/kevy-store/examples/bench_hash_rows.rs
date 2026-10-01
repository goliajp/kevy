//! Hash-row writes and the row-packing conversion, timed.
//!
//! Every path a hash write takes — create, overwrite, add, delete, integer
//! update, table growth — plus the packed-row conversion a table
//! declaration runs over existing rows, and a packed row's round trip
//! through the cold tier. Absolute nanoseconds drift with host load; run
//! two builds back to back on one quiet machine and compare those.
//!
//! Run: `cargo run -p kevy-store --example bench_hash_rows --release`

use std::time::Instant;

use kevy_bench::{bench, black_box};
use kevy_store::Store;
use kevy_store::packed_row::ColumnNames;

const ROWS: usize = 100_000;
const SAMPLES: usize = 31;
const INNER: usize = 20_000;

fn key(i: usize) -> Vec<u8> {
    format!("row:{i:08}").into_bytes()
}

/// The capacity decomposition's row: four short fields and a 900-byte pad.
fn load(s: &mut Store, pad: &[u8]) -> Vec<Vec<u8>> {
    let keys: Vec<Vec<u8>> = (0..ROWS).map(key).collect();
    for (i, k) in keys.iter().enumerate() {
        let id = i.to_string();
        s.hset(
            k,
            &[
                (b"id".as_slice(), id.as_bytes()),
                (b"status", b"active"),
                (b"score", b"42"),
                (b"ts", b"1700000000"),
                (b"pad", pad),
            ],
        )
        .expect("a hash");
    }
    keys
}

fn names(cols: &[&[u8]]) -> ColumnNames {
    cols.iter().map(|c| c.to_vec()).collect()
}

fn writes(pad: &[u8]) {
    let mut s = Store::new();
    let keys = load(&mut s, pad);
    let mut i = 0usize;
    bench(SAMPLES, INNER, || {
        let k = &keys[i % ROWS];
        i += 1;
        s.del(&[k.as_slice()]);
        let id = i.to_string();
        black_box(s.hset(
            k,
            &[
                (b"id".as_slice(), id.as_bytes()),
                (b"status", b"active"),
                (b"score", b"42"),
                (b"ts", b"1700000000"),
                (b"pad", pad),
            ],
        ))
        .expect("a hash");
    })
    .report("del + hset 5-field row");
    bench(SAMPLES, INNER, || {
        let k = &keys[i % ROWS];
        i += 1;
        black_box(s.hset(k, &[(b"status".as_slice(), b"idle".as_slice())])).expect("a hash");
    })
    .report("hset overwrite 1 field");
    bench(SAMPLES, INNER, || {
        let k = &keys[i % ROWS];
        i += 1;
        s.hset(k, &[(b"extra-field-name-long".as_slice(), pad)]).expect("a hash");
        black_box(s.hdel(k, &[b"extra-field-name-long".as_slice()])).expect("a hash");
    })
    .report("hset new field + hdel");
    bench(SAMPLES, INNER, || {
        let k = &keys[i % ROWS];
        i += 1;
        black_box(s.hincrby(k, b"score", 1)).expect("an integer");
    })
    .report("hincrby");
    let fields: Vec<Vec<u8>> = (0..32).map(|f| format!("field{f}").into_bytes()).collect();
    bench(SAMPLES, INNER / 32, || {
        for f in &fields {
            s.hset(b"grow", &[(f.as_slice(), b"value-of-thirty-bytes-........".as_slice())])
                .expect("a hash");
        }
        black_box(s.del(&[b"grow".as_slice()]));
    })
    .report("grow to 32 fields + del");
}

/// One conversion pass over every row, the backfill's shape: per-row
/// median of `reps` passes over fresh stores.
fn pack_pass(pad: &[u8], cols: &ColumnNames, reps: usize) -> u64 {
    let mut per_row = Vec::with_capacity(reps);
    for _ in 0..reps {
        let mut s = Store::new();
        s.set_packed_rows(true);
        let keys = load(&mut s, pad);
        let t = Instant::now();
        for k in &keys {
            s.pack_row(k, cols);
        }
        per_row.push(t.elapsed().as_nanos() as u64 / ROWS as u64);
    }
    per_row.sort_unstable();
    per_row[reps / 2]
}

fn packing() {
    let every = names(&[b"id", b"status", b"score", b"ts", b"pad"]);
    let pad = vec![b'p'; 900];
    println!("  {:<30} median {:>8} ns/row", "pack_row, packable rows", pack_pass(&pad, &every, 7));
    // a declared table that does not name `pad`: the rows cannot pack
    let partial = names(&[b"id", b"status", b"score", b"ts"]);
    let mut s = Store::new();
    s.set_packed_rows(true);
    let keys = load(&mut s, &pad);
    let mut i = 0usize;
    bench(SAMPLES, INNER, || {
        let k = &keys[i % ROWS];
        i += 1;
        s.pack_row(k, &partial);
    })
    .report("pack_row, unpackable rows");
}

fn cold_round_trip() {
    let dir = kevy_tmpdir::TmpDir::new("bench-hash-rows-cold");
    let mut s = Store::new();
    s.enable_tiering(dir.path(), u64::MAX).expect("tiering");
    s.set_packed_rows(true);
    let pad = vec![b'p'; 900];
    let keys = load(&mut s, &pad);
    let every = names(&[b"id", b"status", b"score", b"ts", b"pad"]);
    for k in &keys {
        s.pack_row(k, &every);
    }
    let mut i = 0usize;
    bench(SAMPLES, INNER / 4, || {
        let k = &keys[i % ROWS];
        i += 1;
        s.debug_force_demote(k);
        // the second read promotes
        black_box(s.hget(k, b"status").expect("a hash"));
        black_box(s.hget(k, b"status").expect("a hash"));
    })
    .report("packed row demote + promote");
}

fn main() {
    println!("hash rows, {ROWS} rows of 4 short fields and a pad\n");
    println!("== writes, 900-byte pad ==");
    writes(&[b'p'; 900]);
    println!("== writes, 64-byte pad ==");
    writes(&[b'p'; 64]);
    println!("== packing ==");
    packing();
    println!("== cold tier ==");
    cold_round_trip();
}
