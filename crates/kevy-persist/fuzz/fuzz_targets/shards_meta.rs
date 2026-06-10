//! Fuzz `kevy_persist::read_shards_meta` on arbitrary file bytes.
//!
//! `shards.meta` is a trust boundary: bring-up parses whatever is on disk
//! (possibly written by an older/newer kevy, an embedded-store v1 layout,
//! or a corrupted volume) and a wrong answer re-shards a whole data dir.
//! Invariants asserted across arbitrary inputs:
//!
//!   * never panics, terminates promptly
//!   * any successfully parsed meta round-trips: write_shards_meta then
//!     read_shards_meta returns the identical value (parse/print fixpoint,
//!     so a reshard decision is stable across restarts)

#![no_main]

use kevy_persist::{read_shards_meta, write_shards_meta};
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    let path = std::env::temp_dir().join(format!(
        "kevy-shards-meta-fuzz-{}-{}.meta",
        std::process::id(),
        data.len(),
    ));
    {
        let mut f = std::fs::File::create(&path).expect("create temp meta");
        f.write_all(data).expect("write temp meta");
    }
    if let Some(meta) = read_shards_meta(&path) {
        // Parse/print fixpoint: what we accepted must re-read identically.
        write_shards_meta(&path, meta).expect("rewrite meta");
        assert_eq!(read_shards_meta(&path), Some(meta), "meta round-trip drift");
    }
    let _ = std::fs::remove_file(&path);
});
