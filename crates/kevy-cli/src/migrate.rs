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

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use kevy_resp::{Reply, encode_command_borrowed};
use kevy_resp_client::RespClient;

const PIPELINE: usize = 512;

/// Run `export` — walk the keyspace (optionally under `prefix`) and
/// write rebuild frames to `out_path`. Returns exported key count.
/// What an export did — including what it did NOT do. The skipped map
/// is the half that used to be invisible: a type with no rebuild verb
/// produced no frames, no error and no mention, so a migration could
/// report success while leaving a whole type behind.
pub struct Export {
    /// Keys whose frames are in the file.
    pub keys: u64,
    /// Type name -> keys left out because nothing here rebuilds them.
    pub skipped: std::collections::BTreeMap<Vec<u8>, u64>,
}

/// Walk the keyspace (optionally under `prefix`) and write rebuild
/// frames to `out_path`, reporting both what went in and what could
/// not.
pub fn run_export(
    client: &mut RespClient,
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

/// Emit one key's rebuild frames. `Ok(None)` = nothing written; the
/// type name comes back when the reason was "no rebuild verb", so the
/// caller can report what it is leaving behind rather than count it as
/// a key that happened to vanish.
fn export_key(
    client: &mut RespClient,
    key: &[u8],
    out: &mut impl Write,
) -> io::Result<Option<Option<Vec<u8>>>> {
    match rebuild_frames(client, key, key)? {
        Rebuilt::Frames(frame) => {
            out.write_all(&frame)?;
            Ok(Some(None))
        }
        Rebuilt::Vanished => Ok(None),
        Rebuilt::UnsupportedType(ty) => Ok(Some(Some(ty))),
    }
}

/// Why a key produced no frames. The two reasons are not the same and
/// were indistinguishable: a vanished key is a race the walk expects,
/// an unsupported type is data the caller is about to leave behind.
pub(crate) enum Rebuilt {
    /// The rebuild frames, ready to write.
    Frames(Vec<u8>),
    /// The key was gone between SCAN and read — expected, uncounted.
    Vanished,
    /// The type has no rebuild verb here. Carries the type name so the
    /// caller can tell someone rather than skip in silence.
    UnsupportedType(Vec<u8>),
}

/// Read `key` and produce DEL+rebuild frames addressed to `dst`
/// (`dst == key` for export; a re-prefixed name for copy-prefix —
/// the server has no COPY verb, so copying IS read+rebuild).
pub(crate) fn rebuild_frames(
    client: &mut RespClient,
    key: &[u8],
    dst: &[u8],
) -> io::Result<Rebuilt> {
    let ty = match client.request_borrowed(&[b"TYPE", key])? {
        Reply::Simple(t) => t,
        _ => return Ok(Rebuilt::Vanished),
    };
    if ty == b"none" {
        return Ok(Rebuilt::Vanished);
    }
    let mut frame = Vec::new();
    // DEL first: replay rebuilds from scratch (idempotence for
    // append-shaped verbs like RPUSH).
    encode_command_borrowed(&mut frame, &[b"DEL", dst]);
    match encode_body(client, key, dst, &ty, &mut frame)? {
        Some(()) => {}
        None => return Ok(Rebuilt::Vanished),
    }
    if frame.len() == encoded_del_len(dst) {
        return Ok(Rebuilt::UnsupportedType(ty));
    }
    append_ttl_frame(client, key, dst, &mut frame)?;
    Ok(Rebuilt::Frames(frame))
}

/// The `DEL <dst>` prologue's encoded length — how `rebuild_frames`
/// tells "the body wrote nothing" from "the body wrote frames".
fn encoded_del_len(dst: &[u8]) -> usize {
    let mut probe = Vec::new();
    encode_command_borrowed(&mut probe, &[b"DEL", dst]);
    probe.len()
}

/// Append the type's rebuild verbs to `frame`. `None` = the key
/// vanished mid-read; leaving `frame` untouched = no verb for this
/// type, which the caller turns into `UnsupportedType`.
fn encode_body(
    client: &mut RespClient,
    key: &[u8],
    dst: &[u8],
    ty: &[u8],
    frame: &mut Vec<u8>,
) -> io::Result<Option<()>> {
    match ty {
        b"string" => {
            let Reply::Bulk(v) = client.request_borrowed(&[b"GET", key])? else {
                return Ok(None);
            };
            encode_command_borrowed(frame, &[b"SET", dst, &v]);
        }
        b"hash" => {
            let Some(items) = fetch_bulks(client, &[b"HGETALL", key])? else {
                return Ok(None);
            };
            encode_multi(frame, b"HSET", dst, &items);
        }
        b"list" => {
            let Some(vals) = fetch_bulks(client, &[b"LRANGE", key, b"0", b"-1"])? else {
                return Ok(None);
            };
            encode_multi(frame, b"RPUSH", dst, &vals);
        }
        b"set" => {
            let Some(ms) = fetch_bulks(client, &[b"SMEMBERS", key])? else {
                return Ok(None);
            };
            encode_multi(frame, b"SADD", dst, &ms);
        }
        b"zset" => {
            let zrange: &[&[u8]] = &[b"ZRANGE", key, b"0", b"-1", b"WITHSCORES"];
            let Some(flat) = fetch_bulks(client, zrange)? else {
                return Ok(None);
            };
            encode_zadd(frame, dst, &flat);
        }
        // Streams and anything added later: no rebuild verb here. The
        // caller reports it by name — a migration that drops a type
        // must say which one.
        _ => return Ok(Some(())),
    }
    Ok(Some(()))
}

/// Issue `cmd` and unwrap its Array reply into bulk payloads.
/// `None` when the reply isn't an array or the array is empty (the key
/// vanished / changed type between TYPE and read).
fn fetch_bulks(client: &mut RespClient, cmd: &[&[u8]]) -> io::Result<Option<Vec<Vec<u8>>>> {
    let Reply::Array(items) = client.request_borrowed(cmd)? else {
        return Ok(None);
    };
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        items
            .into_iter()
            .filter_map(|r| if let Reply::Bulk(b) = r { Some(b) } else { None })
            .collect(),
    ))
}

/// Encode `<verb> <dst> <vals…>` onto `frame` (HSET / RPUSH / SADD).
fn encode_multi(frame: &mut Vec<u8>, verb: &[u8], dst: &[u8], vals: &[Vec<u8>]) {
    let mut argv: Vec<&[u8]> = vec![verb, dst];
    argv.extend(vals.iter().map(Vec::as_slice));
    encode_command_borrowed(frame, &argv);
}

/// Encode `ZADD <dst> score member …` onto `frame`.
/// ZADD wants score member; ZRANGE gives member score.
fn encode_zadd(frame: &mut Vec<u8>, dst: &[u8], flat: &[Vec<u8>]) {
    let mut argv: Vec<&[u8]> = vec![b"ZADD", dst];
    for pair in flat.chunks(2) {
        if pair.len() == 2 {
            argv.push(&pair[1]);
            argv.push(&pair[0]);
        }
    }
    encode_command_borrowed(frame, &argv);
}

/// TTL rides as an absolute PEXPIREAT follow-up.
fn append_ttl_frame(
    client: &mut RespClient,
    key: &[u8],
    dst: &[u8],
    frame: &mut Vec<u8>,
) -> io::Result<()> {
    if let Reply::Int(ms) = client.request_borrowed(&[b"PTTL", key])?
        && ms > 0
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis() as i64;
        encode_command_borrowed(frame, &[b"PEXPIREAT", dst, (now + ms).to_string().as_bytes()]);
    }
    Ok(())
}

/// Import stats.
pub struct ImportReport {
    /// Commands sent successfully.
    pub sent: u64,
    /// -ERR replies (counted, not fatal unless `strict`).
    pub errors: u64,
    /// Byte offset reached in the source file.
    pub offset: u64,
}

/// Run `import` — stream `src` (a RESP command file) into the server,
/// `PIPELINE` commands per batch. The progress file `<src>.progress`
/// records the safely-applied byte offset after every batch (fsynced);
/// `resume` starts there. Idempotent replay.
pub fn run_import(
    client: &mut RespClient,
    src: &Path,
    resume: bool,
    strict: bool,
) -> io::Result<ImportReport> {
    let (mut progress, start) = open_progress(src, resume)?;
    let mut f = File::open(src)?;
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
                flush_batch(client, &pending[..batch_bytes], batch_cmds, strict, &mut report)?;
                pending.drain(..batch_bytes);
                write_progress(&mut progress, report.offset)?;
                batch_bytes = 0;
                batch_cmds = 0;
            }
        }
    }
    if batch_cmds > 0 {
        flush_batch(client, &pending[..batch_bytes], batch_cmds, strict, &mut report)?;
        write_progress(&mut progress, report.offset)?;
    }
    Ok(report)
}

fn flush_batch(
    client: &mut RespClient,
    raw: &[u8],
    n: usize,
    strict: bool,
    report: &mut ImportReport,
) -> io::Result<()> {
    let replies = client.pipeline_raw(raw, n)?;
    for r in replies {
        if let Reply::Error(e) = r {
            report.errors += 1;
            if strict {
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
fn open_progress(src: &Path, resume: bool) -> io::Result<(File, u64)> {
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

fn write_progress(f: &mut File, offset: u64) -> io::Result<()> {
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
