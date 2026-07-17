//! AOF tests: append/replay, group commit, magic header, and corruption
//! tolerance. Split from `tests.rs` (snapshot tests); the rewrite family
//! lives in `tests_rewrite.rs`. All three honor the 500-LOC house rule.

use super::*;
use crate::tests::temp_file;
use std::fs::OpenOptions;
use std::io::Write;

fn cmd(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

#[test]
fn aof_append_and_replay() {
    let path = temp_file("aof");
    {
        let mut aof = Aof::open(&path, Fsync::Always).unwrap();
        aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
        aof.append(&cmd(&[b"INCR", b"a"])).unwrap();
        aof.append(&cmd(&[b"SET", b"b", b"hello world"])).unwrap();
    }
    let mut got: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| got.push(args)).unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], cmd(&[b"SET", b"a", b"1"]));
    assert_eq!(got[1], cmd(&[b"INCR", b"a"]));
    assert_eq!(got[2], cmd(&[b"SET", b"b", b"hello world"]));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aof_group_commit_defers_then_flushes() {
    // appendfsync=always group commit: inside begin_group/end_group the
    // appends buffer (one fsync per batch), and end_group makes them all
    // durable BEFORE the caller sends replies. Guards the durable-before-
    // reply contract the reactor relies on.
    let path = temp_file("aofgroup");
    let mut aof = Aof::open(&path, Fsync::Always).unwrap();
    aof.begin_group();
    aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
    aof.append(&cmd(&[b"SET", b"b", b"2"])).unwrap();
    aof.append(&cmd(&[b"SET", b"c", b"3"])).unwrap();
    // Mid-group, before end_group: the batch is still buffered, not on disk.
    let mut mid: Vec<Argv> = Vec::new();
    replay_aof(&path, |a| mid.push(a)).unwrap();
    assert!(mid.is_empty(), "group commit must defer until end_group, saw {}", mid.len());
    // end_group does the single fsync for the whole batch.
    aof.end_group().unwrap();
    let mut after: Vec<Argv> = Vec::new();
    replay_aof(&path, |a| after.push(a)).unwrap();
    assert_eq!(after, vec![
        cmd(&[b"SET", b"a", b"1"]),
        cmd(&[b"SET", b"b", b"2"]),
        cmd(&[b"SET", b"c", b"3"]),
    ]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aof_truncated_tail_ignored() {
    let path = temp_file("aoftail");
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
    }
    // Simulate a crash mid-append: a partial frame at the end.
    let mut f = OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"*2\r\n$3\r\nSET\r\n$5\r\nhal").unwrap(); // truncated
    drop(f);

    let mut got: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| got.push(args)).unwrap();
    assert_eq!(got, vec![cmd(&[b"SET", b"a", b"1"])]); // only the complete frame
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aof_truncate_clears() {
    let path = temp_file("aoftrunc");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    aof.append(&cmd(&[b"SET", b"a", b"1"])).unwrap();
    aof.truncate().unwrap();
    aof.append(&cmd(&[b"SET", b"b", b"2"])).unwrap();
    drop(aof);

    let mut got: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| got.push(args)).unwrap();
    assert_eq!(got, vec![cmd(&[b"SET", b"b", b"2"])]); // pre-truncate write gone
    let _ = std::fs::remove_file(&path);
}

#[test]
fn replay_missing_file_is_ok() {
    let path = temp_file("nofile");
    let mut n = 0;
    replay_aof(&path, |_| n += 1).unwrap();
    assert_eq!(n, 0);
}

/// A production incident shape: SSH stderr ("Warning: Permanently
/// added …") got redirected into the AOF by a deploy
/// pipeline. RESP has an *inline* form (space-tokenized for raw-typed
/// PING / DEBUG), so the junk does parse into commands — but kevy
/// must NOT panic, and the dispatcher above will reject the bogus
/// verbs at -ERR level. This test pins the lower-level guarantee:
/// replay returns Ok and processes every byte without crash, even
/// when the bytes are clearly not anything we ever wrote.
#[test]
fn replay_aof_with_ssh_stderr_head_does_not_panic() {
    use std::io::Write;
    let path = temp_file("ssh_warning_head");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(
        b"Warning: Permanently added 't02.golia.jp' (ED25519) to the list of known hosts.\r\n",
    ).unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n").unwrap();
    drop(f);
    let mut n = 0;
    replay_aof(&path, |_| n += 1).expect("replay must not panic on junk input");
    // The SSH stderr line and the trailing SET both produce "commands"
    // at the parse layer (inline + multibulk). The summary line on
    // stderr will show this count — operations notices it's wrong.
    assert!(n >= 2, "saw at least the inline junk + the SET, got {n}");
    let _ = std::fs::remove_file(&path);
}

/// A *real* malformed RESP frame (`*` header with non-numeric count)
/// triggers the parser's Err path — and exercises the "WARN with
/// hex preview" branch of replay_aof. The clean prefix replays;
/// the corrupt frame + everything after is dropped; the function
/// still returns Ok.
/// New AOFs created by `Aof::open` carry the `KEVYAOF1\n`
/// magic header. `replay_aof` strips it before parsing RESP.
#[test]
fn fresh_aof_has_magic_header_and_replays_cleanly() {
    use std::io::Read;
    let path = temp_aof("magic-fresh");
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))
            .unwrap();
    }
    // Inspect bytes on disk: first 9 must be the (v2) magic.
    let mut f = std::fs::File::open(&path).unwrap();
    let mut buf = [0u8; 9];
    f.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"KEVYAOF2\n");
    // Replay: should see exactly one command, not the magic.
    let mut seen: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| seen.push(args)).unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].get(0).unwrap(), b"SET");
    let _ = std::fs::remove_file(&path);
}

/// Pre-1.2.0 AOFs ("legacy bare-RESP", no magic header) still replay
/// — `replay_aof` only consumes the magic if it sees it. Backward-
/// compat is non-negotiable for the install base.
#[test]
fn legacy_aof_without_magic_still_replays() {
    use std::io::Write;
    let path = temp_aof("magic-legacy");
    // Build a bare-RESP AOF by hand (no magic prefix). Mirrors what a
    // 1.0/1.1 server would have written.
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n").unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nx\r\n$1\r\ny\r\n").unwrap();
    drop(f);
    let mut seen: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| seen.push(args)).unwrap();
    assert_eq!(seen.len(), 2);
    let _ = std::fs::remove_file(&path);
}

/// `Aof::truncate` rewrites the file to just the magic header — so
/// post-truncate replays still identify the file as kevy-managed.
#[test]
fn truncate_preserves_magic_header() {
    use std::io::Read;
    let path = temp_aof("magic-truncate");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    aof.append(&Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))
        .unwrap();
    aof.truncate().unwrap();
    assert_eq!(aof.size_bytes(), 9);
    drop(aof);
    let mut f = std::fs::File::open(&path).unwrap();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"KEVYAOF2\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn replay_aof_with_real_corrupt_frame_keeps_prefix() {
    use std::io::Write;
    let path = temp_file("real_corrupt_mid");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n").unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$1\r\n2\r\n").unwrap();
    // Multi-bulk start byte (`*`) with non-numeric length → Err path.
    f.write_all(b"*BAD\r\n").unwrap();
    f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nc\r\n$1\r\n3\r\n").unwrap();
    drop(f);
    let mut n = 0;
    replay_aof(&path, |_| n += 1).expect("replay must not panic on corrupt frame");
    assert_eq!(n, 2, "prefix replays; corrupt frame stops the loop; tail dropped");
    let _ = std::fs::remove_file(&path);
}

// Regression: a crash (VM/process kill with un-fsynced EverySec pages)
// leaves a zero-filled region after the last complete frame. Before the
// fix, `Aof::open` appended *after* the zeros, so the next replay stopped
// at the zeros and silently orphaned everything written after reopen —
// the exact "expo Android REOPEN returned ∅" bug. `Aof::open` must
// truncate the torn tail so the reopen's appends stay replayable.
#[test]
fn aof_open_truncates_crash_zero_tail_so_reopen_appends_survive() {
    let path = temp_file("aof-zero-tail");
    // Simulate the post-crash file: magic + one good frame + a zero region
    // (the crash-lost EverySec tail, size journaled but data un-flushed).
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(AOF_MAGIC).unwrap();
        write_multibulk(&mut f, &cmd(&[b"SET", b"k", b"v"])).unwrap();
        f.write_all(&[0u8; 128]).unwrap(); // torn/zeroed tail
    }
    // Reopen (what the next process boot does) and append fresh writes.
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        aof.append(&cmd(&[b"SET", b"k2", b"v2"])).unwrap();
    }
    // Replay must see BOTH the pre-crash frame and the post-reopen one —
    // the append landed contiguous with the prefix, not behind the zeros.
    let mut got: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| got.push(args)).unwrap();
    assert_eq!(
        got,
        vec![cmd(&[b"SET", b"k", b"v"]), cmd(&[b"SET", b"k2", b"v2"])],
        "torn zero-tail truncated on open; reopen's append is replayable"
    );
    let _ = std::fs::remove_file(&path);
}

pub(crate) fn temp_aof(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("kevy-{name}-{uniq}.aof"));
    p
}

// ---- snapshot feed-cursor header -------------------------------------------

#[test]
fn snapshot_cursor_roundtrip_and_legacy_none() {
    let dir = kevy_tmpdir::unique_dir("aof");
    let mut store = kevy_store::Store::new();
    store.set(b"k", b"v".to_vec(), None, false, false);

    // v5: cursor written + read back; entries load fine.
    let p5 = dir.join("v5.rdb");
    {
        let mut f = std::fs::File::create(&p5).unwrap();
        crate::write_snapshot_to_with_cursor(&store, &mut f, Some((3, 42))).unwrap();
    }
    assert_eq!(crate::read_snapshot_cursor(&p5).unwrap(), Some((3, 42)));
    let mut loaded = kevy_store::Store::new();
    crate::load_snapshot(&mut loaded, &p5).unwrap();
    assert_eq!(loaded.get(b"k").unwrap().unwrap().as_ref(), b"v");

    // legacy v4 (no cursor): None + loads fine.
    let p4 = dir.join("v4.rdb");
    crate::save_snapshot(&store, &p4).unwrap();
    assert_eq!(crate::read_snapshot_cursor(&p4).unwrap(), None);
    let mut loaded4 = kevy_store::Store::new();
    crate::load_snapshot(&mut loaded4, &p4).unwrap();
    assert_eq!(loaded4.get(b"k").unwrap().unwrap().as_ref(), b"v");
    let _ = std::fs::remove_dir_all(&dir);
}

// The dropped tail must be PRESERVED, not destroyed: after a corrupt frame
// mid-file the region behind it is mostly well-formed frames (a production
// incident dropped 231 MB over one bad frame), and the quarantine copy is
// the only path back to those bytes. Exact-content check.
#[test]
fn aof_open_quarantines_the_dropped_tail_bytes_exactly() {
    let path = temp_file("aof-quarantine");
    let torn = b"*3\r\n$3\r\nSET\r\n$1\r\nq\r\n$5\r\nhal"; // frame cut mid-bulk
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(AOF_MAGIC).unwrap();
        write_multibulk(&mut f, &cmd(&[b"SET", b"k", b"v"])).unwrap();
        f.write_all(torn).unwrap();
    }
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        aof.append(&cmd(&[b"SET", b"k2", b"v2"])).unwrap();
    }
    // Main file: torn bytes gone, both good frames replayable.
    let mut got: Vec<Argv> = Vec::new();
    replay_aof(&path, |args| got.push(args)).unwrap();
    assert_eq!(got, vec![cmd(&[b"SET", b"k", b"v"]), cmd(&[b"SET", b"k2", b"v2"])]);
    // Quarantine: exists next to the AOF, holds EXACTLY the dropped bytes.
    let dir = path.parent().unwrap();
    let stem = path.file_name().unwrap().to_string_lossy().into_owned();
    let qfile = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(&stem) && n.contains(".corrupt-quarantine.")
        })
        .expect("quarantine file must exist");
    let qbytes = std::fs::read(qfile.path()).unwrap();
    assert_eq!(qbytes, torn, "quarantine holds exactly the dropped region");
    let _ = std::fs::remove_file(qfile.path());
    let _ = std::fs::remove_file(&path);
}

// The three auto-rewrite triggers, independently: growth (the classic
// pct/min pair), the absolute byte cap (fires below the growth threshold —
// the rule that stops a 2.2 GB log waiting for 4.4 GB), and staleness
// (interval elapsed AND the log actually grew).
#[test]
fn rewrite_due_three_triggers() {
    let path = temp_file("aof-policy");
    let mut aof = Aof::open(&path, Fsync::No).unwrap();
    for _ in 0..64 {
        aof.append(&cmd(&[b"SET", b"k", b"value-payload-0123456789"])).unwrap();
    }
    let off = RewritePolicy { pct: 0, min_size: 0, bytes: 0, interval_secs: 0 };
    assert!(!aof.rewrite_due(off), "all-zero policy = auto-rewrite off");

    // Growth: baseline is the open size (magic only), so any pct fires once
    // past min_size; a sky-high min_size gates it back off.
    let growth = RewritePolicy { pct: 100, min_size: 1, bytes: 0, interval_secs: 0 };
    assert!(aof.rewrite_due(growth));
    let gated = RewritePolicy { pct: 100, min_size: u64::MAX, bytes: 0, interval_secs: 0 };
    assert!(!aof.rewrite_due(gated));

    // Absolute cap: fires regardless of growth ratio or min_size gate.
    let cap = RewritePolicy { pct: 0, min_size: 0, bytes: 64, interval_secs: 0 };
    assert!(aof.rewrite_due(cap));
    let cap_high = RewritePolicy { pct: 0, min_size: 0, bytes: u64::MAX, interval_secs: 0 };
    assert!(!aof.rewrite_due(cap_high));

    // Staleness: elapsed >= interval AND grown. Freshly opened + appended:
    // a 1 s interval hasn't elapsed yet; after sleeping past it, it fires.
    let stale = RewritePolicy { pct: 0, min_size: 0, bytes: 0, interval_secs: 1 };
    assert!(!aof.rewrite_due(stale), "interval not yet elapsed");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(aof.rewrite_due(stale), "elapsed + grown fires");
    let _ = std::fs::remove_file(&path);
}


// The v2 upgrade contract: a v1 (3.x) file keeps appending v1 so the file
// stays single-format, replays fine, and its FIRST rewrite flips it to the
// checksummed v2 envelope — after which a payload bit-flip is DETECTED
// instead of silently replayed (the exact hole crashgate's payload-flip
// cell caught in the v1 format).
#[test]
fn v1_file_upgrades_on_rewrite_and_v2_detects_bit_rot() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let path = temp_aof("v1-upgrade");
    // Hand-build a v1 file (what a 3.x kevy wrote).
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"KEVYAOF1\n").unwrap();
        write_multibulk(&mut f, &cmd(&[b"SET", b"k", b"v1-era"])).unwrap();
    }
    // Open appends IN v1 (no format mixing), and the file still replays.
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        aof.append(&cmd(&[b"SET", b"k2", b"still-v1"])).unwrap();
    }
    let mut got: Vec<Argv> = Vec::new();
    replay_aof(&path, |a| got.push(a)).unwrap();
    assert_eq!(got.len(), 2);
    let head = std::fs::read(&path).unwrap();
    assert!(head.starts_with(b"KEVYAOF1\n"), "append must not change the format");

    // Rewrite: the output is v2 and replays identically.
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        let mut store = Store::new();
        store.set(b"k", b"v1-era".to_vec(), None, false, false);
        store.set(b"k2", b"still-v1".to_vec(), None, false, false);
        aof.rewrite_from(&store).unwrap();
        // Post-rewrite appends are v2 records.
        aof.append(&cmd(&[b"SET", b"k3", b"v2-era"])).unwrap();
    }
    let head = std::fs::read(&path).unwrap();
    assert!(head.starts_with(b"KEVYAOF2\n"), "rewrite upgrades the format");
    let mut got2: Vec<Argv> = Vec::new();
    replay_aof(&path, |a| got2.push(a)).unwrap();
    assert!(got2.iter().any(|a| a.get(1) == Some(b"k3")), "v2 append replays");

    // Bit-rot detection: flip one byte inside the LAST record's payload —
    // v2 stops at the checksum mismatch instead of replaying the taint.
    let size = std::fs::metadata(&path).unwrap().len();
    {
        let mut f = std::fs::OpenOptions::new().write(true).read(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(size - 3)).unwrap();
        let mut b = [0u8; 1];
        f.read_exact(&mut b).unwrap();
        f.seek(SeekFrom::Start(size - 3)).unwrap();
        f.write_all(&[b[0] ^ 0xFF]).unwrap();
    }
    let mut got3: Vec<Argv> = Vec::new();
    let report = replay_aof(&path, |a| got3.push(a)).unwrap();
    assert!(report.corrupt, "a flipped payload byte must be DETECTED");
    assert!(
        !got3.iter().any(|a| a.get(1) == Some(b"k3")),
        "the tainted record must not replay"
    );
    let _ = std::fs::remove_file(&path);
}

// The 231MB-incident shape in miniature: one bad record mid-file, a pile
// of well-formed records behind it. Strict replay drops the good tail
// (prefix-only); replay_aof_resync hops the bad record and gets every
// good record back, reporting the skipped range — recovery goes from
// prefix-only to all-but-the-bad-frame.
#[test]
fn resync_recovers_the_good_tail_behind_a_corrupt_record() {
    let path = temp_aof("resync");
    {
        let mut aof = Aof::open(&path, Fsync::No).unwrap();
        for i in 0..10 {
            aof.append(&cmd(&[b"SET", format!("pre{i}").as_bytes(), b"v"])).unwrap();
        }
    }
    let before_bad = std::fs::metadata(&path).unwrap().len();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        // A structurally-plausible record whose checksum lies.
        f.write_all(&12u32.to_le_bytes()).unwrap();
        f.write_all(&0xDEAD_BEEFu32.to_le_bytes()).unwrap();
        f.write_all(b"garbagegarba").unwrap();
    }
    {
        // Hand-append the post-damage records (a normal open would repair
        // the file first and destroy the setup).
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let mut scratch = Vec::new();
        for i in 0..10 {
            crate::record::write_record_multibulk(
                &mut f,
                &cmd(&[b"SET", format!("post{i}").as_bytes(), b"v"]),
                &mut scratch,
            )
            .unwrap();
        }
    }

    // Strict: the good tail is gone with the bad record.
    let mut strict: Vec<Argv> = Vec::new();
    let r = replay_aof(&path, |a| strict.push(a)).unwrap();
    assert!(r.corrupt);
    assert_eq!(strict.len(), 10, "strict replay stops at the bad record");

    // Resync: everything but the bad record comes back.
    let mut resynced: Vec<Argv> = Vec::new();
    let r = crate::replay_aof_resync(&path, |a| resynced.push(a)).unwrap();
    assert!(r.corrupt, "resync still reports the corruption");
    assert_eq!(resynced.len(), 20, "the good tail is recovered");
    assert!(resynced.iter().any(|a| a.get(1) == Some(b"post9")));
    assert_eq!(r.resynced_ranges.len(), 1);
    let (a, b) = r.resynced_ranges[0];
    assert_eq!(a, before_bad, "skip starts at the bad record");
    assert_eq!(b - a, 20, "skip covers exactly the bad record's bytes");
    let _ = std::fs::remove_file(&path);
}
