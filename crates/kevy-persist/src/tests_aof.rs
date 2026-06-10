//! AOF tests: append/replay, group commit, magic header, and corruption
//! tolerance. Split from `tests.rs` (snapshot tests); the rewrite family
//! lives in `tests_rewrite.rs`. All three honor the 500-LOC house rule.

use super::*;
use crate::tests::temp_file;
use std::fs::OpenOptions;

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

/// The mailrs prod incident shape: SSH stderr ("Warning: Permanently
/// added 't02.golia.jp' …") got redirected into the AOF by a deploy
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
/// New AOFs created by `Aof::open` carry the v1.2.0 `KEVYAOF1\n`
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
    // Inspect bytes on disk: first 9 must be the magic.
    let mut f = std::fs::File::open(&path).unwrap();
    let mut buf = [0u8; 9];
    f.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"KEVYAOF1\n");
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
    assert_eq!(buf, b"KEVYAOF1\n");
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

pub(crate) fn temp_aof(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("kevy-{name}-{uniq}.aof"));
    p
}
