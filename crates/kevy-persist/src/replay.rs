//! AOF replay path — turns a byte stream back into the command series
//! that wrote it. Carved out of lib.rs to keep the production cap honest;
//! the public re-export in lib.rs keeps the API surface unchanged.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::replay_walk::{ReplayStop, Sink, walk_v2};
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
    replay_with(path, false, false, Sink::Owned(&mut apply))
}

/// The replay behind [`replay_aof`], [`replay_aof_quiet`] and
/// [`replay_aof_resync`], handing each frame to `apply` by reference. The
/// frame's buffers are reused for the next one, so an `apply` that only
/// reads the frame costs no allocation per frame; one that keeps it takes
/// it with `std::mem::take`.
///
/// ```
/// let dir = std::env::temp_dir().join(format!("replay-doc-{}", std::process::id()));
/// std::fs::create_dir_all(&dir).unwrap();
/// let path = dir.join("doc.aof");
/// let mut aof = kevy_persist::Aof::open(&path, kevy_persist::Fsync::No).unwrap();
/// aof.append(&kevy_persist::Argv::from(vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()]))
///     .unwrap();
/// drop(aof);
///
/// let mut verbs = Vec::new();
/// let report = kevy_persist::replay_aof_in_place(&path, false, true, |frame| {
///     verbs.push(frame[0].to_vec());
/// })
/// .unwrap();
/// assert_eq!(report.commands, 1);
/// assert_eq!(verbs, [b"SET".to_vec()]);
/// # std::fs::remove_dir_all(&dir).unwrap();
/// ```
pub fn replay_aof_in_place<F: FnMut(&mut Argv)>(
    path: &Path,
    resync: bool,
    quiet_info: bool,
    mut apply: F,
) -> io::Result<ReplayReport> {
    replay_with(path, resync, quiet_info, Sink::InPlace(&mut apply))
}

fn replay_with(
    path: &Path,
    resync: bool,
    quiet_info: bool,
    mut sink: Sink<'_>,
) -> io::Result<ReplayReport> {
    // v2 files stream record-by-record: peak memory is O(largest record),
    // not O(file) — a 2 GB log replays in a container the old read_to_end
    // would have OOM'd. v1 (legacy) keeps the whole-file read; its first
    // rewrite upgrades it out of that world.
    if matches!(sniff_format(path)?, crate::AofFormat::V2) {
        return stream_v2(path, Some(sink), resync, quiet_info);
    }
    let mut data = Vec::new();
    match File::open(path) {
        Ok(mut f) => {
            f.read_to_end(&mut data)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ReplayReport::default()),
        Err(e) => return Err(e),
    }
    replay_v1_slice(path, &data, &mut sink, quiet_info)
}

/// [`replay_aof`] (or, with `resync`, [`replay_aof_resync`]) with the
/// informational summary lines suppressed. For embedded callers that
/// receive the same numbers through a metric sink: the data path has
/// taken over, so the stderr line would be a duplicate. The corrupt-frame
/// WARN still prints unconditionally — it is an incident signal, not
/// information, and does not share this switch.
pub fn replay_aof_quiet<F: FnMut(Argv)>(
    path: &Path,
    resync: bool,
    mut apply: F,
) -> io::Result<ReplayReport> {
    replay_with(path, resync, true, Sink::Owned(&mut apply))
}

/// The v1 frame loop: parse-apply until clean end, truncated tail, or a
/// corrupt frame. Advances `pos`; returns the stop and the applied count.
fn v1_walk(data: &[u8], pos: &mut usize, sink: &mut Sink<'_>) -> (ReplayStop, u64) {
    let total = data.len();
    let mut replayed: u64 = 0;
    let mut args = Argv::default();
    let stop = loop {
        if *pos >= total {
            break ReplayStop::Clean;
        }
        match kevy_resp::parse_command_into(&data[*pos..], &mut args) {
            Ok(Some(consumed)) => {
                sink.deliver(&mut args);
                *pos += consumed;
                replayed += 1;
            }
            Ok(None) => break ReplayStop::TruncatedTail,
            Err(e) => break ReplayStop::CorruptFrame(format!("{e:?}")),
        }
    };
    (stop, replayed)
}

/// The v1 (bare-RESP) replay walk over a whole-file slice.
fn replay_v1_slice(
    path: &Path,
    data: &[u8],
    sink: &mut Sink<'_>,
    quiet_info: bool,
) -> io::Result<ReplayReport> {
    let total = data.len();
    if total == 0 {
        return Ok(ReplayReport::default());
    }
    // Replay wall-clock — AOF is an unbounded resource, so its replay time is
    // too; surfacing it gives operators a baseline to watch it grow.
    let start = std::time::Instant::now();
    // v1 (`KEVYAOF1\n`) or legacy bare-RESP (pre-1.2.0, parses from 0).
    let mut pos =
        if data.starts_with(crate::aof::AOF_MAGIC) { crate::aof::AOF_MAGIC.len() } else { 0 };
    let (stop, replayed) = v1_walk(data, &mut pos, sink);
    let elapsed_ms = start.elapsed().as_millis();
    let corrupt = matches!(stop, ReplayStop::CorruptFrame(_));
    // quiet_info silences only the informational outcomes; the corrupt
    // WARN is an incident signal and always prints.
    if corrupt || !quiet_info {
        log_replay_summary(path, total, pos, replayed, &data[pos.min(total)..], stop, elapsed_ms);
    }
    Ok(ReplayReport {
        commands: replayed,
        bytes: total as u64,
        replayed_bytes: pos as u64,
        dropped_bytes: (total - pos) as u64,
        zero_tail: 0,
        corrupt,
        resynced_ranges: Vec::new(),
    })
}

/// [`replay_aof`], best-effort: on a corrupt v2 record, scan forward for
/// the next valid record (length + CRC + exactly-one-command all agree —
/// a false accept needs a ~2⁻³² checksum collision AND a clean parse) and
/// keep replaying. The skipped ranges come back in
/// [`ReplayReport::resynced_ranges`]; `corrupt` stays true so the caller
/// still alerts. A real incident dropped a 231 MB tail of well-formed
/// frames over one bad record — this is the lane that gets them back.
/// v1 files have no checksums to anchor on: they replay strictly here
/// too (their first rewrite upgrades them into resync's world).
pub fn replay_aof_resync<F: FnMut(Argv)>(path: &Path, mut apply: F) -> io::Result<ReplayReport> {
    replay_with(path, true, false, Sink::Owned(&mut apply))
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
    /// truncated by [`crate::Aof::open`]. The zero tail is not among them.
    pub dropped_bytes: u64,
    /// Zeros from the last record to the end of the file: the unused part
    /// of a mapped log's preallocation, cut off without quarantine.
    pub zero_tail: u64,
    /// True when the stop was a corrupt frame (vs a clean end or a
    /// partial trailing frame).
    pub corrupt: bool,
    /// Byte ranges resync skipped over ([`replay_aof_resync`] only):
    /// each is a corrupt region between two valid records. Empty under
    /// the strict replay.
    pub resynced_ranges: Vec<(u64, u64)>,
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

/// Resync runs on any non-clean stop, not only a corrupt frame.
///
/// A length field that lies — legal but larger than the bytes that remain —
/// makes the walk call the stop a torn tail while the cause is corruption,
/// and everything behind it is lost with `corrupt` left false. The two cases
/// are pinned in `tests_aof.rs`: `resync_recovers_the_good_tail_behind_a_
/// lying_length` and `resync_on_a_genuine_torn_tail_adds_nothing`.
fn stream_v2(
    path: &Path,
    mut apply: Option<Sink<'_>>,
    resync: bool,
    quiet_info: bool,
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
    let mut w = walk_v2(&mut r, magic.len() as u64, &mut apply)?;
    let corrupt = matches!(w.stop, ReplayStop::CorruptFrame(_));
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    // Any non-clean stop, not only the ones the walk calls corrupt — see
    // `resync_on_a_non_clean_stop` above for why a torn tail is asked too.
    if resync && !matches!(w.stop, ReplayStop::Clean) {
        crate::replay_resync::resync_fallback(path, &mut w, &mut apply, &mut ranges)?;
    }
    // A skipped range IS corruption — it is the only thing resync skips.
    let corrupt = corrupt || !ranges.is_empty();
    // after a resync hop the records past the damage were applied one by
    // one, so the open transaction no longer marks where the log settles
    let end = if ranges.is_empty() { w.settled_end() } else { w.pos };
    if apply.is_some() {
        log_v2_outcome(path, &w, total, end, corrupt, quiet_info, start.elapsed().as_millis());
    }
    Ok(ReplayReport {
        commands: w.replayed,
        bytes: total,
        replayed_bytes: end,
        dropped_bytes: total.saturating_sub(end).saturating_sub(w.zero_tail),
        zero_tail: w.zero_tail,
        corrupt,
        resynced_ranges: ranges,
    })
}

/// The valid prefix's length and the zero tail after it (always 0 for v1).
/// The replay's summary lines. `quiet_info` silences only the
/// informational outcomes; the corrupt WARN always prints.
fn log_v2_outcome(
    path: &Path,
    w: &crate::replay_walk::V2Walk,
    total: u64,
    end: u64,
    corrupt: bool,
    quiet_info: bool,
    elapsed_ms: u128,
) {
    if !quiet_info && end < w.pos {
        crate::replay_log::log_open_transaction(path, w.pos - end);
    }
    if corrupt || !quiet_info {
        let preview = &w.preview[..w.preview_len];
        log_replay_summary(
            path,
            total as usize,
            w.pos as usize,
            w.replayed,
            preview,
            w.stop.clone(),
            elapsed_ms,
        );
    }
}

pub(crate) fn valid_prefix_len_of_file(path: &Path, resync: bool) -> io::Result<(u64, u64)> {
    // v2 streams (O(largest record) memory — the same walk replay does, so
    // the truncation point and the replay stop can never disagree). Under
    // resync the point is "after the LAST recoverable record", so interior
    // corruption stays put and only trailing garbage is repaired away.
    if matches!(sniff_format(path)?, crate::AofFormat::V2) {
        let r = stream_v2(path, None, resync, false)?;
        return Ok((r.replayed_bytes, r.zero_tail));
    }
    let mut data = Vec::new();
    match File::open(path) {
        Ok(mut f) => {
            f.read_to_end(&mut data)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e),
    }
    Ok((valid_prefix_len(&data) as u64, 0))
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

use crate::replay_log::log_replay_summary;
