//! The downgrade window is a state, and this is the query that reads it.
//!
//! From the smix dogfood report (2026-07-26): `UPGRADING.md` documents
//! precisely when a 4.x data directory can still be opened by a 3.x
//! binary — v1 files stay v1 until their first rewrite — but the format
//! was `pub(crate)`, so an embedder's own diagnostics could not answer
//! "can my user still swap the binary back?". smix shipped its
//! CHANGELOG declaring the window closed unconditionally, because a
//! conditional, observable state it could not observe is
//! indistinguishable from a closed one.

#![cfg(feature = "persist")]

use kevy_embedded::{Config, Store};

/// A fresh directory writes v2 from the first byte: the window is
/// closed before it was ever open, and the query says so.
#[test]
fn a_fresh_directory_is_not_downgradeable() {
    let dir = kevy_tmpdir::TmpDir::new("aoffmt-fresh");
    let store = Store::open(Config::default().with_persist(dir.path())).expect("open");
    store.set(b"k", b"v").expect("set");
    assert_eq!(store.downgradeable_to_v3(), Some(false), "new files speak v2");
}

/// A memory-only store has no files to downgrade — the question has no
/// meaning, and the answer is None rather than a guess either way.
#[test]
fn a_memory_store_answers_none() {
    let store = Store::open(Config::default()).expect("open");
    assert_eq!(store.downgradeable_to_v3(), None);
}

/// The real journey: a directory carrying a 3.x-era v1 file is
/// downgradeable exactly until the first rewrite upgrades it — the
/// window smix's `doctor` wanted to report, end to end.
#[test]
fn a_v1_directory_is_downgradeable_until_the_first_rewrite() {
    let dir = kevy_tmpdir::TmpDir::new("aoffmt-v1");
    // Fabricate what a 3.x binary leaves behind: a v1 AOF holding one
    // SET, in the single-shard layout (aof-0.aof).
    let v1 = {
        let mut f = b"KEVYAOF1\n".to_vec();
        f.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
        f
    };
    std::fs::write(dir.path().join("aof-0.aof"), &v1).expect("plant the 3.x file");

    let store = Store::open(Config::default().with_persist(dir.path()).with_shards(1))
        .expect("a 3.x directory opens on 4.x — the UPGRADING promise");
    assert_eq!(store.get(b"k").expect("get").as_deref(), Some(&b"v"[..]), "replayed");
    assert_eq!(
        store.downgradeable_to_v3(),
        Some(true),
        "appends stay v1: the window is open and now visibly so"
    );

    // Appends keep the file v1 — the window survives ordinary writes.
    store.set(b"k2", b"v2").expect("append");
    assert_eq!(store.downgradeable_to_v3(), Some(true));

    // The first rewrite is the documented one-way step.
    store.rewrite_aof().expect("rewrite");
    assert_eq!(
        store.downgradeable_to_v3(),
        Some(false),
        "the rewrite upgraded the file; the window is closed and says so"
    );
}
