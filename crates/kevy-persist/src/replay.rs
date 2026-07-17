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
    // Format sniff: v2 (checksummed record envelopes), v1 (`KEVYAOF1\n`),
    // or legacy bare-RESP (pre-1.2.0, parses from position 0).
    let is_v2 = data.starts_with(crate::record::AOF2_MAGIC);
    let mut pos = if is_v2 || data.starts_with(crate::aof::AOF_MAGIC) {
        crate::record::AOF2_MAGIC.len()
    } else {
        0
    };
    let mut replayed: u64 = 0;
    let stop = loop {
        if pos >= total {
            break ReplayStop::Clean;
        }
        if is_v2 {
            match crate::record::next_record(&data, pos) {
                crate::record::RecordStep::Ok { payload, consumed } => {
                    match kevy_resp::parse_command(payload) {
                        // A record must hold exactly one complete command —
                        // anything else means the envelope lies.
                        Ok(Some((args, used))) if used == payload.len() => {
                            apply(args);
                            pos += consumed;
                            replayed += 1;
                        }
                        _ => break ReplayStop::CorruptFrame(String::from(
                            "checksummed record does not hold exactly one command",
                        )),
                    }
                }
                crate::record::RecordStep::Truncated => break ReplayStop::TruncatedTail,
                crate::record::RecordStep::Corrupt(why) => {
                    break ReplayStop::CorruptFrame(String::from(why));
                }
            }
        } else {
            match kevy_resp::parse_command(&data[pos..]) {
                Ok(Some((args, consumed))) => {
                    apply(args);
                    pos += consumed;
                    replayed += 1;
                }
                Ok(None) => break ReplayStop::TruncatedTail,
                Err(e) => break ReplayStop::CorruptFrame(format!("{e:?}")),
            }
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
/// Which encoding the file at `path` speaks, by magic sniff. Missing or
/// short files count as V2 (they're about to be created fresh).
pub(crate) fn sniff_format(path: &Path) -> io::Result<crate::AofFormat> {
    let mut head = [0u8; 9];
    match File::open(path) {
        Ok(mut f) => match f.read_exact(&mut head) {
            Ok(()) if head == *crate::record::AOF2_MAGIC => Ok(crate::AofFormat::V2),
            Ok(()) => Ok(crate::AofFormat::V1),
            Err(_) => Ok(crate::AofFormat::V2),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(crate::AofFormat::V2),
        Err(e) => Err(e),
    }
}

pub(crate) fn valid_prefix_len_of_file(path: &Path) -> io::Result<u64> {
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
                crate::record::RecordStep::Ok { consumed, .. } => pos += consumed,
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
