//! AOF replay path — turns a byte stream back into the command series
//! that wrote it. Carved out of lib.rs to keep the production cap honest;
//! the public re-export in lib.rs keeps the API surface unchanged.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use kevy_resp::Argv;

/// Replay the command log at `path`, calling `apply` for each complete command.
///
/// Always emits a one-line summary to stderr when the file has any bytes,
/// so operators can immediately see how many commands were replayed and
/// how many bytes were dropped (truncated tail or parse error). A
/// production incident once went unnoticed through a 70-day silent
/// failure window because this summary was opt-in — making it
/// always-on is cheap (one line per restart) and turns
/// silent-empty-store from a multi-hour outage into a one-line log
/// hit.
///
/// Three outcomes:
///
/// * **Clean** — every byte consumed by valid RESP frames. Logs
///   `replayed N commands from M bytes`.
/// * **Truncated tail** — a crash mid-append left a partial frame. The
///   prefix is intact and replays normally; the trailing partial bytes
///   are silently OK. Logs `replayed N commands; trailing K bytes were
///   a partial frame (crash mid-append, recoverable)`.
/// * **Corrupt frame** — parser hit invalid bytes mid-file. The prefix
///   replayed; the tail (including the bad frame) is dropped. Logs a
///   loud WARN with the byte offset, parser error, and a hex+ascii
///   preview of the bad region. Common cause: deploy pipeline wrote
///   non-kevy bytes (e.g. SSH stderr) into the AOF path.
///
/// A missing file is treated as an empty log (returns Ok(()) silently,
/// no log line).
///
/// Note: RESP has an *inline* form (space-separated tokens) for backward
/// compatibility, so a stderr line like `Warning: Permanently added ...`
/// will parse as a valid (if nonsense) command. The summary line is the
/// signal — an unexpected count of replayed commands at boot is the
/// operator's cue to inspect the AOF byte-by-byte.
pub fn replay_aof<F: FnMut(Argv)>(path: &Path, mut apply: F) -> io::Result<ReplayReport> {
    // v2 files stream record-by-record: peak memory is O(largest record),
    // not O(file) — a 2 GB log replays in a container the old read_to_end
    // would have OOM'd. v1 (legacy) keeps the whole-file read; its first
    // rewrite upgrades it out of that world.
    if matches!(sniff_format(path)?, crate::AofFormat::V2) {
        return stream_v2(path, Some(&mut apply));
    }
    let mut data = Vec::new();
    match File::open(path) {
        Ok(mut f) => {
            f.read_to_end(&mut data)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ReplayReport::default()),
        Err(e) => return Err(e),
    }
    let total = data.len();
    if total == 0 {
        return Ok(ReplayReport::default());
    }
    // Replay wall-clock — AOF is an unbounded resource, so its replay time is
    // too; surfacing it gives operators a baseline to watch it grow.
    let start = std::time::Instant::now();
    // v1 (`KEVYAOF1\n`) or legacy bare-RESP (pre-1.2.0, parses from 0).
    let mut pos = if data.starts_with(crate::aof::AOF_MAGIC) {
        crate::aof::AOF_MAGIC.len()
    } else {
        0
    };
    let mut replayed: u64 = 0;
    let stop = loop {
        if pos >= total {
            break ReplayStop::Clean;
        }
        match kevy_resp::parse_command(&data[pos..]) {
            Ok(Some((args, consumed))) => {
                apply(args);
                pos += consumed;
                replayed += 1;
            }
            Ok(None) => break ReplayStop::TruncatedTail,
            Err(e) => break ReplayStop::CorruptFrame(format!("{e:?}")),
        }
    };
    let elapsed_ms = start.elapsed().as_millis();
    let corrupt = matches!(stop, ReplayStop::CorruptFrame(_));
    log_replay_summary(path, total, pos, replayed, &data[pos.min(total)..], stop, elapsed_ms);
    Ok(ReplayReport {
        commands: replayed,
        bytes: total as u64,
        replayed_bytes: pos as u64,
        dropped_bytes: (total - pos) as u64,
        corrupt,
    })
}

/// What one [`replay_aof`] pass restored — and, crucially, what it could
/// NOT: `dropped_bytes` and `corrupt` are the machine-readable form of the
/// WARN line, so a host can turn "the AOF lost bytes at boot" into an
/// alert instead of a needle in stderr (the 3-day silent-loss incident was
/// exactly this signal going unwatched).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ReplayReport {
    /// Commands re-applied.
    pub commands: u64,
    /// Total file size in bytes (before any repair).
    pub bytes: u64,
    /// Bytes actually replayed (the valid prefix).
    pub replayed_bytes: u64,
    /// Bytes past the last complete frame — dropped, then quarantined and
    /// truncated by [`crate::Aof::open`].
    pub dropped_bytes: u64,
    /// True when the stop was a corrupt frame (vs a clean end or a
    /// partial trailing frame).
    pub corrupt: bool,
}

/// Byte length of the AOF at `path` up to and including the last
/// **complete** RESP frame (after the magic header). Trailing bytes
/// beyond it — a partial frame from a crash mid-append, or a zero-filled
/// region from a crash with un-fsynced pages — are not replayable.
/// [`crate::aof::Aof::open`] truncates the file to this before the first
/// append, so new writes stay contiguous with the replayable prefix
/// instead of landing behind the torn tail (where the next replay would
/// stop and silently orphan them). Uses the same parser as
/// [`replay_aof`], so the truncation point and the replay stop point can
/// never disagree. A missing file is length 0.
/// Which encoding the file at `path` speaks, by magic sniff. Only an
/// exact `KEVYAOF2\n` head is V2; short, missing, or other files are V1
/// (the lenient legacy path — an empty or stub file replays as nothing
/// there, and a fresh `Aof::open` stamps its own v2 magic before this
/// ever matters).
pub(crate) fn sniff_format(path: &Path) -> io::Result<crate::AofFormat> {
    let mut head = [0u8; 9];
    match File::open(path) {
        Ok(mut f) => match f.read_exact(&mut head) {
            Ok(()) if head == *crate::record::AOF2_MAGIC => Ok(crate::AofFormat::V2),
            _ => Ok(crate::AofFormat::V1),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(crate::AofFormat::V1),
        Err(e) => Err(e),
    }
}

/// Stream a v2 AOF record-by-record: 8-byte header, payload into a
/// reusable buffer, CRC check, exactly-one-command parse, apply. Peak
/// memory is O(largest record). `apply = None` is the valid-prefix walk —
/// same stops, no side effects, and no summary line.
fn stream_v2(
    path: &Path,
    mut apply: Option<&mut dyn FnMut(Argv)>,
) -> io::Result<ReplayReport> {
    use std::io::BufReader;
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ReplayReport::default()),
        Err(e) => return Err(e),
    };
    let total = file.metadata().map_or(0, |m| m.len());
    let mut r = BufReader::with_capacity(256 * 1024, file);
    let mut magic = [0u8; 9];
    r.read_exact(&mut magic)?; // caller sniffed v2, the magic is present
    let start = std::time::Instant::now();
    let mut pos = magic.len() as u64;
    let mut replayed = 0u64;
    let mut payload: Vec<u8> = Vec::new();
    let mut preview = [0u8; 16];
    let mut preview_len = 0usize;
    let stop = loop {
        let mut header = [0u8; 8];
        match read_fully(&mut r, &mut header) {
            Ok(0) => break ReplayStop::Clean,
            Ok(n) if n < header.len() => break ReplayStop::TruncatedTail,
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(header[..4].try_into().unwrap());
        let crc = u32::from_le_bytes(header[4..].try_into().unwrap());
        if len == 0 || len > crate::record::MAX_RECORD {
            preview_len = 8.min(preview.len());
            preview[..preview_len].copy_from_slice(&header[..preview_len]);
            break ReplayStop::CorruptFrame(String::from("record length out of range"));
        }
        payload.clear();
        payload.resize(len as usize, 0);
        match read_fully(&mut r, &mut payload) {
            Ok(n) if n < payload.len() => break ReplayStop::TruncatedTail,
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        if crate::crc32c::crc32c(&payload) != crc {
            preview_len = payload.len().min(16);
            preview[..preview_len].copy_from_slice(&payload[..preview_len]);
            break ReplayStop::CorruptFrame(String::from("record checksum mismatch"));
        }
        match kevy_resp::parse_command(&payload) {
            Ok(Some((args, used))) if used == payload.len() => {
                if let Some(f) = apply.as_deref_mut() {
                    f(args);
                }
                pos += 8 + u64::from(len);
                replayed += 1;
            }
            _ => {
                preview_len = payload.len().min(16);
                preview[..preview_len].copy_from_slice(&payload[..preview_len]);
                break ReplayStop::CorruptFrame(String::from(
                    "checksummed record does not hold exactly one command",
                ));
            }
        }
    };
    let elapsed_ms = start.elapsed().as_millis();
    let corrupt = matches!(stop, ReplayStop::CorruptFrame(_));
    if apply.is_some() {
        log_replay_summary(
            path,
            total as usize,
            pos as usize,
            replayed,
            &preview[..preview_len],
            stop,
            elapsed_ms,
        );
    }
    Ok(ReplayReport {
        commands: replayed,
        bytes: total,
        replayed_bytes: pos,
        dropped_bytes: total.saturating_sub(pos),
        corrupt,
    })
}

/// `read_exact` that reports a clean-vs-partial short read instead of
/// erroring: returns the bytes actually read (0 = clean EOF at a record
/// boundary, partial = torn tail).
fn read_fully<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

pub(crate) fn valid_prefix_len_of_file(path: &Path) -> io::Result<u64> {
    // v2 streams (O(largest record) memory — the same walk replay does, so
    // the truncation point and the replay stop can never disagree).
    if matches!(sniff_format(path)?, crate::AofFormat::V2) {
        return Ok(stream_v2(path, None)?.replayed_bytes);
    }
    let mut data = Vec::new();
    match File::open(path) {
        Ok(mut f) => {
            f.read_to_end(&mut data)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    }
    Ok(valid_prefix_len(&data) as u64)
}

/// Offset after the last complete frame in `data` (magic-aware). Mirrors
/// the `replay_aof` parse loop, minus the `apply`.
fn valid_prefix_len(data: &[u8]) -> usize {
    let total = data.len();
    let is_v2 = data.starts_with(crate::record::AOF2_MAGIC);
    let mut pos = if is_v2 || data.starts_with(crate::aof::AOF_MAGIC) {
        crate::record::AOF2_MAGIC.len()
    } else {
        0
    };
    loop {
        if pos >= total {
            break;
        }
        if is_v2 {
            match crate::record::next_record(data, pos) {
                crate::record::RecordStep::Ok { payload, consumed } => {
                    // Mirror replay's exactly-one-command rule so the
                    // truncation point can never disagree with its stop.
                    match kevy_resp::parse_command(payload) {
                        Ok(Some((_, used))) if used == payload.len() => pos += consumed,
                        _ => break,
                    }
                }
                _ => break,
            }
            continue;
        }
        match kevy_resp::parse_command(&data[pos..]) {
            Ok(Some((_, consumed))) => pos += consumed,
            Ok(None) | Err(_) => break,
        }
    }
    pos
}

/// Outcome of an AOF replay run — drives the summary log shape.
enum ReplayStop {
    Clean,
    TruncatedTail,
    CorruptFrame(String),
}

/// Emit the one-line replay summary. Goes to stderr because kevy-persist
/// has no log-crate dependency (pure-Rust + 0 deps charter); production
/// deployments route stderr to their existing log sink.
fn log_replay_summary(
    path: &Path,
    total: usize,
    pos: usize,
    replayed: u64,
    remainder: &[u8],
    stop: ReplayStop,
    elapsed_ms: u128,
) {
    let display = path.display();
    let dropped = total - pos;
    match stop {
        ReplayStop::Clean => {
            eprintln!(
                "kevy: AOF {display} replayed {replayed} commands from {total} bytes \
                 in {elapsed_ms} ms (clean)"
            );
        }
        ReplayStop::TruncatedTail => {
            eprintln!(
                "kevy: AOF {display} replayed {replayed} commands from {total} bytes \
                 in {elapsed_ms} ms; trailing {dropped} bytes \
                 were a partial frame (crash mid-append, recoverable)"
            );
        }
        ReplayStop::CorruptFrame(err) => {
            let preview = preview_bytes(remainder);
            eprintln!(
                "kevy WARN: AOF {display} replayed {replayed} commands in {elapsed_ms} ms \
                 then hit a corrupt \
                 frame at byte {pos}; dropping the trailing {dropped} bytes \
                 (quarantined before truncation). \
                 Preview: {preview}. Parser error: {err}. \
                 Most common cause: the process was killed mid-append (a torn \
                 frame); less commonly, non-kevy bytes got written into this \
                 file path (e.g. a deploy pipeline redirecting stderr here)."
            );
        }
    }
}

/// Hex + ASCII preview of up to 16 bytes, for diagnostic eprintlns.
fn preview_bytes(b: &[u8]) -> String {
    use std::fmt::Write;
    let n = b.len().min(16);
    let mut hex = String::with_capacity(n * 3);
    let mut ascii = String::with_capacity(n);
    for &x in &b[..n] {
        if !hex.is_empty() {
            hex.push(' ');
        }
        let _ = write!(hex, "{x:02x}");
        ascii.push(if (0x20..0x7f).contains(&x) { x as char } else { '.' });
    }
    format!("hex=[{hex}] ascii=[{ascii}]")
}
