//! Fuzz `kevy_persist::load_snapshot_from` on arbitrary byte streams.
//!
//! This is the malicious-primary / corrupt-file trust boundary: a
//! replica feeds a primary's snapshot bytes straight into the loader
//! (`replication_apply` → `load_snapshot_from`), and a node loads its
//! own on-disk snapshot at startup. A forged element count or bulk
//! length must fail with a plain `io::Error`, never a panic and never
//! an unbounded `with_capacity` / `vec![0; n]` alloc-abort.
//!
//! The asserted property: load must terminate without panicking across
//! arbitrary inputs, INCLUDING a valid magic + version prefix followed
//! by corruption (forged counts/lengths are the interesting shapes).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Prepend the snapshot magic + a plausible version to a fraction of
    // inputs so the fuzzer spends time past the header check, where the
    // count/length-driven allocations live. The rest exercise the
    // header-rejection path.
    let mut framed: Vec<u8> = Vec::with_capacity(data.len() + 9);
    if data.first().is_some_and(|b| b & 1 == 0) {
        framed.extend_from_slice(b"KEVYSNAP");
        framed.push(4); // a valid version
        framed.extend_from_slice(data);
    } else {
        framed.extend_from_slice(data);
    }

    let mut store = kevy_store::Store::new();
    // The property is totality: no panic, no OOM-abort, terminate. A
    // corrupt stream must surface as `Err`, never a crash.
    let _ = kevy_persist::load_snapshot_from(&mut store, std::io::Cursor::new(&framed));
});
