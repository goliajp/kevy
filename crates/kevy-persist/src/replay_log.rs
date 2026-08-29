//! The replay summary line — split from `replay.rs` (500-LOC house rule)
//! and grown one behavior: the informational branches can be silenced.
//!
//! For a long-lived server the summary is a boot banner, one line per
//! lifetime. For an embedded short-lived process it prints on every open
//! — a CLI that opens the store per command pays it per command, into
//! terminals, CI logs, and AI-session transcripts. A caller that has
//! registered a metric sink already receives the same numbers as data,
//! so the open paths pass `quiet_info` there. The corrupt-frame WARN is
//! an incident signal and never silenced — it does not share the switch.

use crate::replay_walk::ReplayStop;
use std::path::Path;

/// Emit the one-line replay summary. Goes to stderr because kevy-persist
/// has no log-crate dependency (pure-Rust + 0 deps charter); production
/// deployments route stderr to their existing log sink. Quiet-mode
/// suppression of the informational branches happens at the call sites
/// (the corrupt-frame WARN is always emitted there).
pub(crate) fn log_replay_summary(
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
        ReplayStop::LengthOutranFile { claimed, available } => {
            outran_warn(&display, replayed, elapsed_ms, pos, claimed, available, dropped);
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
        ascii.push(if (0x20..0x7f).contains(&x) {
            x as char
        } else {
            '.'
        });
    }
    format!("hex=[{hex}] ascii=[{ascii}]")
}

/// The message a length field the file could not honour deserves.
///
/// It used to share `TruncatedTail`'s line, which calls the drop "a partial
/// frame (crash mid-append, recoverable)" — CI printed exactly that for
/// 27,485,178 bytes. A torn append leaves at most one incomplete record.
#[allow(clippy::too_many_arguments)]
fn outran_warn(
    display: &std::path::Display<'_>,
    replayed: u64,
    elapsed_ms: u128,
    pos: usize,
    claimed: u64,
    available: u64,
    dropped: usize,
) {
    eprintln!(
        "kevy WARN: AOF {display} replayed {replayed} commands in {elapsed_ms} ms \
         then hit a record at byte {pos} whose length field claimed {claimed} \
         bytes with only {available} left in the file; the trailing {dropped} \
         bytes were dropped. This is not a partial frame from a crash \
         mid-append — a torn append leaves at most one incomplete record. \
         Turn on `replay_resync` to recover the good records behind it."
    );
}
