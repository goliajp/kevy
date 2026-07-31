//! The SEGMENTED stitch frame, replayed end to end: rows whose write
//! frames precede it come out of the hot layer, a write after it is a
//! revival that stays, replay is idempotent, and a frame naming a
//! segment the manifest does not hold refuses startup by name.

#![cfg(all(feature = "persist", not(target_arch = "wasm32")))]

use std::io::Write;
use std::path::Path;

use kevy_embedded::{Config, Store};

/// Build a sealed segment holding `keys` (payloads irrelevant to the
/// stitch) and register it in the shard-0 manifest.
fn seal_segment(data_dir: &Path, file: &str, keys: &[&[u8]]) {
    let segs = data_dir.join("segs-0");
    std::fs::create_dir_all(&segs).unwrap();
    let mut b = kevy_seg::SegBuilder::create(&segs.join(file)).unwrap();
    let mut sorted: Vec<&[u8]> = keys.to_vec();
    sorted.sort();
    for k in &sorted {
        b.push(k, b"cold-copy").unwrap();
    }
    let meta = b.finish().unwrap();
    let mut m = kevy_seg::Manifest::open(&segs).unwrap();
    m.add(kevy_seg::ManifestEntry {
        file: file.to_string(),
        meta: Vec::new(),
        min_key: meta.min_key,
        max_key: meta.max_key,
        records: meta.records,
    })
    .unwrap();
}

/// Append one raw v2 record (multibulk `args`) to the shard-0 AOF —
/// what the eviction path will do through Aof; tests write it by hand.
fn append_frame(data_dir: &Path, args: &[&[u8]]) {
    let mut payload = Vec::new();
    payload.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        payload.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        payload.extend_from_slice(a);
        payload.extend_from_slice(b"\r\n");
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(data_dir.join("aof-0.aof"))
        .unwrap();
    f.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    f.write_all(&kevy_sys::checksum::crc32c(&payload).to_le_bytes()).unwrap();
    f.write_all(&payload).unwrap();
}

fn append_segmented(data_dir: &Path, file: &str) {
    append_frame(data_dir, &[kevy_persist::SEGMENTED, file.as_bytes()]);
}

fn open(dir: &Path) -> Store {
    Store::open(Config::default().with_persist(dir)).expect("open")
}

fn seed_three_rows(dir: &Path) {
    let s = open(dir);
    s.set(b"row:1", b"v1").unwrap();
    s.set(b"row:2", b"v2").unwrap();
    s.set(b"row:3", b"v3").unwrap();
}

#[test]
fn stitch_evicts_segmented_rows_on_replay() {
    let d = kevy_tmpdir::TmpDir::new("segrep-evict");
    seed_three_rows(d.path());
    seal_segment(d.path(), "b1.seg", &[b"row:1", b"row:2"]);
    append_segmented(d.path(), "b1.seg");

    let s = open(d.path());
    assert_eq!(s.get(b"row:1").unwrap(), None, "evicted row replayed back in");
    assert_eq!(s.get(b"row:2").unwrap(), None, "evicted row replayed back in");
    assert_eq!(s.get(b"row:3").unwrap().unwrap(), b"v3", "unsegmented row lost");
}

#[test]
fn write_after_stitch_is_a_revival_that_stays() {
    let d = kevy_tmpdir::TmpDir::new("segrep-revive");
    seed_three_rows(d.path());
    seal_segment(d.path(), "b1.seg", &[b"row:1", b"row:2"]);
    append_segmented(d.path(), "b1.seg");
    append_frame(d.path(), &[b"SET", b"row:1", b"revived"]);

    let s = open(d.path());
    assert_eq!(s.get(b"row:1").unwrap().unwrap(), b"revived");
    assert_eq!(s.get(b"row:2").unwrap(), None);
}

#[test]
fn replaying_the_stitch_twice_is_idempotent() {
    let d = kevy_tmpdir::TmpDir::new("segrep-idem");
    seed_three_rows(d.path());
    seal_segment(d.path(), "b1.seg", &[b"row:1", b"row:2"]);
    append_segmented(d.path(), "b1.seg");
    append_segmented(d.path(), "b1.seg");

    // First reopen applies both frames; a second reopen replays the
    // same log again on top of the snapshot-less state.
    drop(open(d.path()));
    let s = open(d.path());
    assert_eq!(s.get(b"row:2").unwrap(), None);
    assert_eq!(s.get(b"row:3").unwrap().unwrap(), b"v3");
}

#[test]
fn frame_without_manifest_entry_refuses_startup_by_name() {
    let d = kevy_tmpdir::TmpDir::new("segrep-torn");
    seed_three_rows(d.path());
    // A segment directory exists but the ledger never recorded the
    // segment the AOF claims — the truth set was damaged.
    std::fs::create_dir_all(d.path().join("segs-0")).unwrap();
    append_segmented(d.path(), "ghost.seg");

    let err = Store::open(Config::default().with_persist(d.path()))
        .err()
        .expect("startup must refuse, not drop rows");
    let msg = format!("{err}");
    assert!(msg.contains("ghost.seg"), "refusal must name the segment: {msg}");
}
