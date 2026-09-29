//! Migration toolchain: `export` / `import`.
//!
//! Wire format = a RESP command stream of rebuild frames (SET / HSET /
//! RPUSH / SADD / ZADD / PEXPIREAT) — bidirectionally compatible with
//! `redis-cli --pipe`. Export walks SCAN cursors (per-key
//! point-in-time; SCAN-class consistency). Import pipelines 512
//! commands per batch with a fsynced progress file for `--resume`.
//! Every key's frames start with DEL, so replay REBUILDS the key from
//! scratch — genuinely idempotent for every type (RPUSH would
//! otherwise append on re-import; the round-trip test caught exactly
//! that). No cross-batch atomicity to lose.
//!
//! ```
//! use kevy_cli::migrate::{ImportStart, OnErrorReply, run_export, run_import};
//! use kevy_resp_client::Reply;
//! # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
//! let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
//! # let port_b = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
//! let mut target = kevy_resp_client::RespClient::connect("127.0.0.1", port_b)?;
//! client.request_borrowed(&[b"RPUSH", b"user:1:tags", b"a", b"b"])?;
//! client.request_borrowed(&[b"SET", b"user:1:name", b"ada"])?;
//!
//! let dir = kevy_tmpdir::TmpDir::new("migrate-roundtrip");
//! let file = dir.path().join("users.resp");
//! assert_eq!(run_export(&mut client, Some(&b"user:"[..]), &file)?.keys, 2);
//! // replay is idempotent: every key is rebuilt from scratch, so twice is once
//! for _ in 0..2 {
//!     run_import(&mut target, &file, ImportStart::Fresh, OnErrorReply::Abort)?;
//! }
//! let tags = target.request_borrowed(&[b"LLEN", b"user:1:tags"])?;
//! assert_eq!(tags, Reply::Int(2));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::link::Link;
use kevy_resp::Reply;

mod rebuild;
use rebuild::export_key;
pub(crate) use rebuild::{Rebuilt, rebuild_frames};

const PIPELINE: usize = 512;

/// What an export did — including what it did NOT do. The skipped map
/// is the half that used to be invisible: a type with no rebuild verb
/// produced no frames, no error and no mention, so a migration could
/// report success while leaving a whole type behind.
///
/// ```
/// use kevy_cli::migrate::run_export;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"SET", b"k", b"v"])?;
/// client.request_borrowed(&[b"XADD", b"events", b"*", b"f", b"v"])?;
/// # let dir = kevy_tmpdir::TmpDir::new("migrate");
/// let e = run_export(&mut client, None, &dir.path().join("all.resp"))?;
/// if !e.skipped.is_empty() {
///     eprintln!("left behind: {:?}", e.skipped); // say so rather than report success
/// }
/// assert_eq!(e.keys, 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Export {
    /// Keys whose frames are in the file.
    ///
    /// ```
    /// use kevy_cli::migrate::run_export;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"SET", b"a", b"1"])?;
    /// client.request_borrowed(&[b"HSET", b"b", b"f", b"v"])?;
    /// # let dir = kevy_tmpdir::TmpDir::new("migrate");
    /// assert_eq!(run_export(&mut client, None, &dir.path().join("x.resp"))?.keys, 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub keys: u64,
    /// Type name -> keys left out because nothing here rebuilds them.
    ///
    /// ```
    /// use kevy_cli::migrate::run_export;
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// client.request_borrowed(&[b"XADD", b"events", b"*", b"f", b"v"])?;
    /// # let dir = kevy_tmpdir::TmpDir::new("migrate");
    /// let e = run_export(&mut client, None, &dir.path().join("x.resp"))?;
    /// assert_eq!(e.skipped.get(&b"stream".to_vec()), Some(&1));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub skipped: std::collections::BTreeMap<Vec<u8>, u64>,
}

/// Walk the keyspace (optionally under `prefix`) and write rebuild
/// frames to `out_path`, reporting both what went in and what could
/// not.
///
/// ```
/// use kevy_cli::migrate::run_export;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// client.request_borrowed(&[b"SET", b"user:1", b"ada"])?;
/// client.request_borrowed(&[b"SET", b"order:1", b"open"])?;
/// # let dir = kevy_tmpdir::TmpDir::new("migrate");
/// let file = dir.path().join("users.resp");
/// assert_eq!(run_export(&mut client, Some(&b"user:"[..]), &file)?.keys, 1);
/// // the file is plain RESP, `redis-cli --pipe` compatible: DEL, then the rebuild
/// let text = std::fs::read_to_string(&file)?;
/// assert!(text.starts_with("*2\r\n$3\r\nDEL\r\n$6\r\nuser:1\r\n*3\r\n$3\r\nSET\r\n"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run_export(
    client: &mut dyn Link,
    prefix: Option<&[u8]>,
    out_path: &Path,
) -> io::Result<Export> {
    let mut out = BufWriter::new(File::create(out_path)?);
    let mut cursor: Vec<u8> = b"0".to_vec();
    let mut pattern = prefix.unwrap_or_default().to_vec();
    pattern.push(b'*');
    let mut n = 0u64;
    // Types with no rebuild verb, counted by name. Silence here is how
    // a migration loses a whole type and reports success.
    let mut skipped: std::collections::BTreeMap<Vec<u8>, u64> = Default::default();
    loop {
        let reply =
            client.request_borrowed(&[b"SCAN", &cursor, b"MATCH", &pattern, b"COUNT", b"512"])?;
        let Reply::Array(items) = reply else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "SCAN reply shape"));
        };
        let (Some(Reply::Bulk(next)), Some(Reply::Array(keys))) = (items.first(), items.get(1))
        else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "SCAN reply shape"));
        };
        let next = next.clone();
        for k in keys {
            let Reply::Bulk(key) = k else { continue };
            let key = key.clone();
            match export_key(client, &key, &mut out)? {
                Some(None) => n += 1,
                Some(Some(ty)) => *skipped.entry(ty).or_insert(0u64) += 1,
                None => {}
            }
        }
        cursor = next;
        if cursor == b"0" {
            break;
        }
    }
    out.flush()?;
    Ok(Export { keys: n, skipped })
}

/// Import stats.
///
/// ```
/// use kevy_cli::migrate::{ImportStart, OnErrorReply, run_import};
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// # let dir = kevy_tmpdir::TmpDir::new("migrate");
/// # let src = dir.path().join("cmds.resp");
/// # let mut raw = Vec::new();
/// # for argv in [&[&b"SET"[..], b"a", b"1"][..], &[b"SET", b"b", b"x"], &[b"INCR", b"b"]] {
/// #     kevy_resp::encode_command_borrowed(&mut raw, argv);
/// # }
/// # std::fs::write(&src, raw)?;
/// let r = run_import(&mut client, &src, ImportStart::Fresh, OnErrorReply::Count)?;
/// assert_eq!((r.sent, r.errors), (2, 1));
/// assert_eq!(r.offset, std::fs::metadata(&src)?.len());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct ImportReport {
    /// Commands sent successfully.
    ///
    /// ```
    /// use kevy_cli::migrate::{ImportStart, OnErrorReply, run_import};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # let dir = kevy_tmpdir::TmpDir::new("migrate");
    /// # let src = dir.path().join("cmds.resp");
    /// # let mut raw = Vec::new();
    /// # for argv in [&[&b"SET"[..], b"a", b"1"][..], &[b"SET", b"b", b"x"], &[b"INCR", b"b"]] {
    /// #     kevy_resp::encode_command_borrowed(&mut raw, argv);
    /// # }
    /// # std::fs::write(&src, raw)?;
    /// assert_eq!(run_import(&mut client, &src, ImportStart::Fresh, OnErrorReply::Count)?.sent, 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub sent: u64,
    /// -ERR replies (counted; fatal under [`OnErrorReply::Abort`]).
    ///
    /// ```
    /// use kevy_cli::migrate::{ImportStart, OnErrorReply, run_import};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # let dir = kevy_tmpdir::TmpDir::new("migrate");
    /// # let src = dir.path().join("cmds.resp");
    /// # let mut raw = Vec::new();
    /// # for argv in [&[&b"SET"[..], b"a", b"1"][..], &[b"SET", b"b", b"x"], &[b"INCR", b"b"]] {
    /// #     kevy_resp::encode_command_borrowed(&mut raw, argv);
    /// # }
    /// # std::fs::write(&src, raw)?;
    /// let r = run_import(&mut client, &src, ImportStart::Fresh, OnErrorReply::Count)?;
    /// assert_eq!(r.errors, 1, "INCR on a non-number");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub errors: u64,
    /// Byte offset reached in the source file.
    ///
    /// ```
    /// use kevy_cli::migrate::{ImportStart, OnErrorReply, run_import};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # let dir = kevy_tmpdir::TmpDir::new("migrate");
    /// # let src = dir.path().join("cmds.resp");
    /// # let mut raw = Vec::new();
    /// # for argv in [&[&b"SET"[..], b"a", b"1"][..], &[b"SET", b"b", b"x"], &[b"INCR", b"b"]] {
    /// #     kevy_resp::encode_command_borrowed(&mut raw, argv);
    /// # }
    /// # std::fs::write(&src, raw)?;
    /// let r = run_import(&mut client, &src, ImportStart::Fresh, OnErrorReply::Count)?;
    /// // the whole file was applied, and `<src>.progress` says so for a later --resume
    /// assert_eq!(r.offset, std::fs::metadata(&src)?.len());
    /// let progress = std::fs::read_to_string(dir.path().join("cmds.resp.progress"))?;
    /// assert_eq!(progress, r.offset.to_string());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub offset: u64,
}

/// Where an import starts reading its source file.
///
/// ```
/// use kevy_cli::migrate::ImportStart;
/// // `import --resume`
/// assert_ne!(ImportStart::Resume, ImportStart::default());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ImportStart {
    /// From the first byte, resetting `<src>.progress` before any batch.
    ///
    /// ```
    /// use kevy_cli::migrate::{ImportStart, OnErrorReply, run_import};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # let dir = kevy_tmpdir::TmpDir::new("migrate");
    /// # let src = dir.path().join("cmds.resp");
    /// # let mut raw = Vec::new();
    /// # for argv in [&[&b"SET"[..], b"a", b"1"][..], &[b"SET", b"b", b"x"], &[b"INCR", b"b"]] {
    /// #     kevy_resp::encode_command_borrowed(&mut raw, argv);
    /// # }
    /// # std::fs::write(&src, raw)?;
    /// run_import(&mut client, &src, ImportStart::Fresh, OnErrorReply::Count)?;
    /// // fresh ignores any earlier progress and replays the whole file
    /// let again = run_import(&mut client, &src, ImportStart::Fresh, OnErrorReply::Count)?;
    /// assert_eq!(again.sent + again.errors, 3);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[default]
    Fresh,
    /// From the offset `<src>.progress` recorded (`--resume`).
    ///
    /// ```
    /// use kevy_cli::migrate::{ImportStart, OnErrorReply, run_import};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # let dir = kevy_tmpdir::TmpDir::new("migrate");
    /// # let src = dir.path().join("cmds.resp");
    /// # let mut raw = Vec::new();
    /// # for argv in [&[&b"SET"[..], b"a", b"1"][..], &[b"SET", b"b", b"x"], &[b"INCR", b"b"]] {
    /// #     kevy_resp::encode_command_borrowed(&mut raw, argv);
    /// # }
    /// # std::fs::write(&src, raw)?;
    /// run_import(&mut client, &src, ImportStart::Fresh, OnErrorReply::Count)?;
    /// // resuming a finished import starts at its end and sends nothing
    /// let resumed = run_import(&mut client, &src, ImportStart::Resume, OnErrorReply::Count)?;
    /// assert_eq!(resumed.sent + resumed.errors, 0);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Resume,
}

/// What an import does when the server answers a command with an error.
///
/// ```
/// use kevy_cli::migrate::OnErrorReply;
/// // `import --strict`
/// assert_ne!(OnErrorReply::Abort, OnErrorReply::default());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum OnErrorReply {
    /// Count it in [`ImportReport::errors`] and carry on.
    ///
    /// ```
    /// use kevy_cli::migrate::{ImportStart, OnErrorReply, run_import};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # let dir = kevy_tmpdir::TmpDir::new("migrate");
    /// # let src = dir.path().join("cmds.resp");
    /// # let mut raw = Vec::new();
    /// # for argv in [&[&b"SET"[..], b"a", b"1"][..], &[b"SET", b"b", b"x"], &[b"INCR", b"b"]] {
    /// #     kevy_resp::encode_command_borrowed(&mut raw, argv);
    /// # }
    /// # std::fs::write(&src, raw)?;
    /// let r = run_import(&mut client, &src, ImportStart::Fresh, OnErrorReply::Count)?;
    /// assert_eq!((r.sent, r.errors), (2, 1), "counted, and the import went on");
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[default]
    Count,
    /// Stop with an `InvalidData` error naming the reply (`--strict`).
    ///
    /// ```
    /// use kevy_cli::migrate::{ImportStart, OnErrorReply, run_import};
    /// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
    /// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
    /// # let dir = kevy_tmpdir::TmpDir::new("migrate");
    /// # let src = dir.path().join("cmds.resp");
    /// # let mut raw = Vec::new();
    /// # for argv in [&[&b"SET"[..], b"a", b"1"][..], &[b"SET", b"b", b"x"], &[b"INCR", b"b"]] {
    /// #     kevy_resp::encode_command_borrowed(&mut raw, argv);
    /// # }
    /// # std::fs::write(&src, raw)?;
    /// let err = run_import(&mut client, &src, ImportStart::Fresh, OnErrorReply::Abort).unwrap_err();
    /// assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    /// assert!(err.to_string().starts_with("server error (strict)"));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Abort,
}

/// Run `import` — stream `src` (a RESP command file) into the server,
/// `PIPELINE` commands per batch. The progress file `<src>.progress`
/// records the safely-applied byte offset after every batch (fsynced);
/// [`ImportStart::Resume`] starts there. Idempotent replay.
///
/// ```
/// use kevy_cli::migrate::{ImportStart, OnErrorReply, run_import};
/// use kevy_resp_client::Reply;
/// # let port = include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/serve.rs"));
/// let mut client = kevy_resp_client::RespClient::connect("127.0.0.1", port)?;
/// let dir = kevy_tmpdir::TmpDir::new("import");
/// let path = dir.path().join("dump.resp");
/// let mut raw = Vec::new();
/// kevy_resp::encode_command_borrowed(&mut raw, &[&b"SET"[..], b"greet", b"hello"]);
/// std::fs::write(&path, raw)?;
///
/// let r = run_import(&mut client, &path, ImportStart::Resume, OnErrorReply::Abort)?;
/// println!("{} sent, offset {}", r.sent, r.offset);
/// assert_eq!(client.request_borrowed(&[b"GET", b"greet"])?, Reply::Bulk(b"hello".to_vec()));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn run_import(
    client: &mut dyn Link,
    src: &Path,
    start: ImportStart,
    on_error: OnErrorReply,
) -> io::Result<ImportReport> {
    // the source first: a missing one must not leave a progress file behind
    let mut f = File::open(src)?;
    let (mut progress, start) = open_progress(src, start)?;
    f.seek(SeekFrom::Start(start))?;
    let mut pending: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut report = ImportReport { sent: 0, errors: 0, offset: start };
    let mut chunk = vec![0u8; 1 << 20];
    let mut batch_bytes = 0usize;
    let mut batch_cmds = 0usize;
    loop {
        let n = f.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..n]);
        // carve complete commands off `pending`
        while let Some(used) = command_len(&pending[batch_bytes..]) {
            batch_bytes += used;
            batch_cmds += 1;
            if batch_cmds == PIPELINE {
                flush_batch(client, &pending[..batch_bytes], batch_cmds, on_error, &mut report)?;
                pending.drain(..batch_bytes);
                write_progress(&mut progress, report.offset)?;
                batch_bytes = 0;
                batch_cmds = 0;
            }
        }
    }
    if batch_cmds > 0 {
        flush_batch(client, &pending[..batch_bytes], batch_cmds, on_error, &mut report)?;
        write_progress(&mut progress, report.offset)?;
    }
    Ok(report)
}

fn flush_batch(
    client: &mut dyn Link,
    raw: &[u8],
    n: usize,
    on_error: OnErrorReply,
    report: &mut ImportReport,
) -> io::Result<()> {
    let replies = client.pipeline_raw(raw, n)?;
    for r in replies {
        if let Reply::Error(e) = r {
            report.errors += 1;
            if on_error == OnErrorReply::Abort {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("server error (strict): {}", String::from_utf8_lossy(&e)),
                ));
            }
        } else {
            report.sent += 1;
        }
    }
    report.offset += raw.len() as u64;
    Ok(())
}

/// The progress file, opened, and the offset to start from.
///
/// The name is `<src>.progress`, APPENDED — the doc always said so, but
/// `with_extension` turned dump.kevy into dump.progress, and everything
/// written against the documented name (the migration drill's cleanup
/// included) touched a file this code never read.
///
/// A fresh (non-resume) import resets the file immediately, before any
/// batch. A completed earlier import leaves offset = EOF on disk; kill a
/// fresh import before its first batch lands and that stale EOF
/// survives — a later `--resume` then seeks to the end, imports nothing,
/// and reports success. The migration drill hit exactly that window.
pub(crate) fn open_progress(src: &Path, start: ImportStart) -> io::Result<(File, u64)> {
    let resume = start == ImportStart::Resume;
    let path = {
        let mut os = src.as_os_str().to_owned();
        os.push(".progress");
        std::path::PathBuf::from(os)
    };
    let mut start = 0u64;
    if resume && let Ok(text) = std::fs::read_to_string(&path) {
        start = text.trim().parse().unwrap_or(0);
    }
    let mut f = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    if !resume {
        write_progress(&mut f, 0)?;
    }
    Ok((f, start))
}

pub(crate) fn write_progress(f: &mut File, offset: u64) -> io::Result<()> {
    f.set_len(0)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(offset.to_string().as_bytes())?;
    f.sync_data()
}

/// Crate-visible alias for [`command_len`] (bulk copy counts frames).
pub(crate) fn command_len_pub(b: &[u8]) -> Option<usize> {
    command_len(b)
}

/// Length of one complete RESP command at the head of `b`, or `None`.
fn command_len(b: &[u8]) -> Option<usize> {
    let mut pos = 0usize;
    let line = take_line(b, &mut pos)?;
    if line.first() != Some(&b'*') {
        return None;
    }
    let n: usize = std::str::from_utf8(&line[1..]).ok()?.trim().parse().ok()?;
    for _ in 0..n {
        let hdr = take_line(b, &mut pos)?;
        if hdr.first() != Some(&b'$') {
            return None;
        }
        let len: usize = std::str::from_utf8(&hdr[1..]).ok()?.trim().parse().ok()?;
        if b.len() < pos + len + 2 {
            return None;
        }
        pos += len + 2;
    }
    Some(pos)
}

fn take_line<'b>(b: &'b [u8], pos: &mut usize) -> Option<&'b [u8]> {
    let rest = &b[*pos..];
    let idx = rest.windows(2).position(|w| w == b"\r\n")?;
    let line = &rest[..idx];
    *pos += idx + 2;
    Some(line)
}
