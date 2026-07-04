//! **v2.10** — migration toolchain: `export` / `import` (RFC D1/D2).
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
pub fn run_export(
    client: &mut RespClient,
    prefix: Option<&[u8]>,
    out_path: &Path,
) -> io::Result<u64> {
    let mut out = BufWriter::new(File::create(out_path)?);
    let mut cursor: Vec<u8> = b"0".to_vec();
    let mut pattern = prefix.unwrap_or_default().to_vec();
    pattern.push(b'*');
    let mut n = 0u64;
    loop {
        let reply = client.request_borrowed(&[b"SCAN", &cursor, b"MATCH", &pattern, b"COUNT", b"512"])?;
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
            if export_key(client, &key, &mut out)? {
                n += 1;
            }
        }
        cursor = next;
        if cursor == b"0" {
            break;
        }
    }
    out.flush()?;
    Ok(n)
}

/// Emit one key's rebuild frames. Returns false if the key vanished
/// between SCAN and read (point-in-time per key).
fn export_key(client: &mut RespClient, key: &[u8], out: &mut impl Write) -> io::Result<bool> {
    let ty = match client.request_borrowed(&[b"TYPE", key])? {
        Reply::Simple(t) => t,
        _ => return Ok(false),
    };
    let mut frame = Vec::new();
    // DEL first: replay rebuilds from scratch (idempotence for
    // append-shaped verbs like RPUSH).
    encode_command_borrowed(&mut frame, &[b"DEL", key]);
    match ty.as_slice() {
        b"string" => {
            let Reply::Bulk(v) = client.request_borrowed(&[b"GET", key])? else {
                return Ok(false);
            };
            encode_command_borrowed(&mut frame, &[b"SET", key, &v]);
        }
        b"hash" => {
            let Reply::Array(flat) = client.request_borrowed(&[b"HGETALL", key])? else {
                return Ok(false);
            };
            if flat.is_empty() {
                return Ok(false);
            }
            let mut argv: Vec<&[u8]> = vec![b"HSET", key];
            let items: Vec<Vec<u8>> = flat
                .into_iter()
                .filter_map(|r| if let Reply::Bulk(b) = r { Some(b) } else { None })
                .collect();
            argv.extend(items.iter().map(Vec::as_slice));
            encode_command_borrowed(&mut frame, &argv);
        }
        b"list" => {
            let Reply::Array(items) = client.request_borrowed(&[b"LRANGE", key, b"0", b"-1"])?
            else {
                return Ok(false);
            };
            if items.is_empty() {
                return Ok(false);
            }
            let vals: Vec<Vec<u8>> = items
                .into_iter()
                .filter_map(|r| if let Reply::Bulk(b) = r { Some(b) } else { None })
                .collect();
            let mut argv: Vec<&[u8]> = vec![b"RPUSH", key];
            argv.extend(vals.iter().map(Vec::as_slice));
            encode_command_borrowed(&mut frame, &argv);
        }
        b"set" => {
            let Reply::Array(items) = client.request_borrowed(&[b"SMEMBERS", key])? else {
                return Ok(false);
            };
            if items.is_empty() {
                return Ok(false);
            }
            let ms: Vec<Vec<u8>> = items
                .into_iter()
                .filter_map(|r| if let Reply::Bulk(b) = r { Some(b) } else { None })
                .collect();
            let mut argv: Vec<&[u8]> = vec![b"SADD", key];
            argv.extend(ms.iter().map(Vec::as_slice));
            encode_command_borrowed(&mut frame, &argv);
        }
        b"zset" => {
            let Reply::Array(items) =
                client.request_borrowed(&[b"ZRANGE", key, b"0", b"-1", b"WITHSCORES"])?
            else {
                return Ok(false);
            };
            if items.is_empty() {
                return Ok(false);
            }
            let flat: Vec<Vec<u8>> = items
                .into_iter()
                .filter_map(|r| if let Reply::Bulk(b) = r { Some(b) } else { None })
                .collect();
            // ZADD wants score member; ZRANGE gives member score
            let mut argv: Vec<&[u8]> = vec![b"ZADD", key];
            for pair in flat.chunks(2) {
                if pair.len() == 2 {
                    argv.push(&pair[1]);
                    argv.push(&pair[0]);
                }
            }
            encode_command_borrowed(&mut frame, &argv);
        }
        _ => return Ok(false), // streams etc. — out of the rebuild set
    }
    // TTL rides as an absolute PEXPIREAT follow-up
    if let Reply::Int(ms) = client.request_borrowed(&[b"PTTL", key])?
        && ms > 0
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
            .as_millis() as i64;
        encode_command_borrowed(
            &mut frame,
            &[b"PEXPIREAT", key, (now + ms).to_string().as_bytes()],
        );
    }
    out.write_all(&frame)?;
    Ok(true)
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
    let progress_path = src.with_extension("progress");
    let mut start = 0u64;
    if resume && let Ok(text) = std::fs::read_to_string(&progress_path) {
        start = text.trim().parse().unwrap_or(0);
    }
    let mut f = File::open(src)?;
    f.seek(SeekFrom::Start(start))?;
    let mut pending: Vec<u8> = Vec::with_capacity(1 << 20);
    let mut report = ImportReport { sent: 0, errors: 0, offset: start };
    let mut chunk = vec![0u8; 1 << 20];
    let mut batch_bytes = 0usize;
    let mut batch_cmds = 0usize;
    let mut progress = OpenOptions::new().create(true).write(true).open(&progress_path)?;
    loop {
        let n = f.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..n]);
        // carve complete commands off `pending`
        loop {
            let Some(used) = command_len(&pending[batch_bytes..]) else { break };
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

fn write_progress(f: &mut File, offset: u64) -> io::Result<()> {
    f.set_len(0)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(offset.to_string().as_bytes())?;
    f.sync_data()
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
