//! The SEGMENTED stitch frame, replayed end to end under the row-stub
//! semantics: rows whose write frames precede it phase-change to
//! seg-backed stubs (still fully readable — the segment holds the
//! value), a write after it is a revival that stays hot, replay is
//! idempotent, a row the log never rebuilt is inserted as a stub
//! directly from the segment, and a frame naming a segment the
//! manifest does not hold refuses startup by name.

#![cfg(all(feature = "persist", not(target_arch = "wasm32")))]

use std::io::Write;
use std::path::Path;

use kevy_embedded::{Config, Store};

/// One row to seal: its key and its hash fields.
type RowSpec<'a> = (&'a [u8], &'a [(&'a [u8], &'a [u8])]);

/// tier-codec hash body: [nfields u32] then [flen][f][vlen][v] each.
fn hash_body(fields: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = (fields.len() as u32).to_le_bytes().to_vec();
    for (f, v) in fields {
        out.extend_from_slice(&(f.len() as u32).to_le_bytes());
        out.extend_from_slice(f);
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        out.extend_from_slice(v);
    }
    out
}

/// Seal a segment holding `rows` (key → fields) into the shard-0
/// directory, registered with the row tag under a parsable seq name.
fn seal_row_segment(data_dir: &Path, seq: u32, rows: &[RowSpec<'_>]) -> String {
    let segs = data_dir.join("segs-0");
    std::fs::create_dir_all(&segs).unwrap();
    let file = format!("row-7465-{seq}.seg");
    let mut b = kevy_seg::SegBuilder::create(&segs.join(&file)).unwrap();
    let mut sorted: Vec<_> = rows.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (k, fields) in &sorted {
        b.push(k, &hash_body(fields)).unwrap();
    }
    let meta = b.finish().unwrap();
    let mut m = kevy_seg::Manifest::open(&segs).unwrap();
    m.add(kevy_seg::ManifestEntry {
        file: file.clone(),
        meta: b"rowcold:te".to_vec(),
        min_key: meta.min_key,
        max_key: meta.max_key,
        records: meta.records,
    })
    .unwrap();
    file
}

fn append_frame(data_dir: &Path, args: &[&[u8]]) {
    let mut payload = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        payload.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        payload.extend_from_slice(a);
        payload.extend_from_slice(b"\r\n");
    }
    let mut f = std::fs::OpenOptions::new().append(true).open(data_dir.join("aof-0.aof")).unwrap();
    f.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
    f.write_all(&kevy_sys::checksum::crc32c(&payload).to_le_bytes()).unwrap();
    f.write_all(&payload).unwrap();
}

fn open(dir: &Path) -> Store {
    Store::open(Config::default().with_persist(dir)).expect("open")
}

fn run(s: &Store, argv: &[&[u8]]) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
    let mut out = Vec::new();
    s.dispatch_argv(&owned, &mut out);
    out
}

const F1: &[(&[u8], &[u8])] = &[(b"id", b"row:1"), (b"note", b"first")];
const F2: &[(&[u8], &[u8])] = &[(b"id", b"row:2"), (b"note", b"second")];

fn seed_three_rows(dir: &Path) {
    let s = open(dir);
    for (key, fields) in [(b"row:1", F1), (b"row:2", F2)] {
        let mut argv: Vec<&[u8]> = vec![b"HSET", key];
        for (f, v) in fields {
            argv.push(f);
            argv.push(v);
        }
        run(&s, &argv);
    }
    run(&s, &[b"HSET", b"row:3", b"id", b"row:3", b"note", b"stays hot"]);
}

#[test]
fn stitch_phase_changes_rows_and_reads_still_answer() {
    let d = kevy_tmpdir::TmpDir::new("segrep-stub");
    seed_three_rows(d.path());
    let file = seal_row_segment(d.path(), 0, &[(b"row:1", F1), (b"row:2", F2)]);
    append_frame(d.path(), &[kevy_persist::SEGMENTED, file.as_bytes()]);

    let s = open(d.path());
    // Stitched rows still answer — served from the segment.
    let r1 = String::from_utf8_lossy(&run(&s, &[b"HGETALL", b"row:1"])).into_owned();
    assert!(r1.contains("first"), "stitched row unreadable: {r1}");
    assert_eq!(run(&s, &[b"HGET", b"row:2", b"note"]), b"$6\r\nsecond\r\n".to_vec());
    assert_eq!(run(&s, &[b"EXISTS", b"row:1"]), b":1\r\n".to_vec());
    assert_eq!(run(&s, &[b"TYPE", b"row:1"]), b"+hash\r\n".to_vec());
    // The segment survived startup (it is referenced, not an orphan).
    assert!(d.path().join("segs-0").join(&file).exists(), "referenced segment swept");
}

#[test]
fn write_after_stitch_revives_and_a_log_gap_inserts_stubs() {
    let d = kevy_tmpdir::TmpDir::new("segrep-revive");
    seed_three_rows(d.path());
    let file = seal_row_segment(d.path(), 0, &[(b"row:1", F1), (b"row:2", F2)]);
    append_frame(d.path(), &[kevy_persist::SEGMENTED, file.as_bytes()]);
    append_frame(d.path(), &[b"HSET", b"row:1", b"note", b"revived"]);
    // row:9 was never written in the log — the rewritten-log shape:
    // the stitch must insert its stub straight from the segment.
    let file2 =
        seal_row_segment(d.path(), 1, &[(b"row:9", &[(b"id", b"row:9"), (b"note", b"gap")])]);
    append_frame(d.path(), &[kevy_persist::SEGMENTED, file2.as_bytes()]);

    let s = open(d.path());
    let r1 = String::from_utf8_lossy(&run(&s, &[b"HGETALL", b"row:1"])).into_owned();
    assert!(r1.contains("revived"), "revival lost: {r1}");
    assert!(r1.contains("row:1"), "revival dropped merged fields: {r1}");
    assert_eq!(run(&s, &[b"HGET", b"row:9", b"note"]), b"$3\r\ngap\r\n".to_vec());
}

#[test]
fn replaying_the_stitch_twice_is_idempotent() {
    let d = kevy_tmpdir::TmpDir::new("segrep-idem");
    seed_three_rows(d.path());
    let file = seal_row_segment(d.path(), 0, &[(b"row:1", F1), (b"row:2", F2)]);
    append_frame(d.path(), &[kevy_persist::SEGMENTED, file.as_bytes()]);
    append_frame(d.path(), &[kevy_persist::SEGMENTED, file.as_bytes()]);

    drop(open(d.path()));
    let s = open(d.path());
    assert_eq!(run(&s, &[b"HGET", b"row:2", b"note"]), b"$6\r\nsecond\r\n".to_vec());
    assert_eq!(run(&s, &[b"DBSIZE"]), b":3\r\n".to_vec());
}

#[test]
fn frame_without_manifest_entry_refuses_startup_by_name() {
    let d = kevy_tmpdir::TmpDir::new("segrep-torn");
    seed_three_rows(d.path());
    std::fs::create_dir_all(d.path().join("segs-0")).unwrap();
    append_frame(d.path(), &[kevy_persist::SEGMENTED, b"row-7465-9.seg"]);

    let err = Store::open(Config::default().with_persist(d.path()))
        .err()
        .expect("startup must refuse, not drop rows");
    let msg = format!("{err}");
    assert!(msg.contains("row-7465-9.seg"), "refusal must name the segment: {msg}");
}

#[test]
fn orphan_segments_sweep_but_referenced_ones_stay() {
    let d = kevy_tmpdir::TmpDir::new("segrep-orphan");
    seed_three_rows(d.path());
    let referenced = seal_row_segment(d.path(), 0, &[(b"row:1", F1)]);
    append_frame(d.path(), &[kevy_persist::SEGMENTED, referenced.as_bytes()]);
    // Sealed but the crash hit before its frame: rows replay hot,
    // nothing references it, the sweep reclaims it.
    let orphan = seal_row_segment(d.path(), 1, &[(b"row:2", F2)]);

    let s = open(d.path());
    let segs = d.path().join("segs-0");
    assert!(segs.join(&referenced).exists(), "referenced segment swept");
    assert!(!segs.join(&orphan).exists(), "orphan survived the sweep");
    // The orphan's row replayed hot and still answers.
    assert_eq!(run(&s, &[b"HGET", b"row:2", b"note"]), b"$6\r\nsecond\r\n".to_vec());
}
