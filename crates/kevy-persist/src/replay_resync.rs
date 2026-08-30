//! The resync arm of the v2 replay — split from `replay.rs` to keep
//! both files under the 500-LOC house rule. See
//! [`crate::replay::replay_aof_resync`] for the contract.

use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::Path;

use kevy_resp::Argv;

use crate::replay_walk::{ReplayStop, V2Walk};

/// The resync arm of [`stream_v2`]: corruption is the rare path and the
/// rescan needs random access, so slice-load the remainder (O(damaged
/// remainder) memory, only on damaged files) and hop from valid record
/// to valid record.
pub(crate) fn resync_fallback(
    path: &Path,
    w: &mut V2Walk,
    apply: &mut Option<&mut dyn FnMut(Argv)>,
    ranges: &mut Vec<(u64, u64)>,
) -> io::Result<()> {
    let mut rest = Vec::new();
    let mut f = File::open(path)?;
    f.seek(io::SeekFrom::Start(w.pos))?;
    f.read_to_end(&mut rest)?;
    let (local_end, extra, ended_torn) = resync_slice(&rest, w.pos, apply, ranges);
    w.replayed += extra;
    w.pos += local_end;
    // Re-tag for the summary: the file WAS corrupt (the report keeps that
    // truth), but the tail came back — the WARN carries the skipped ranges.
    w.stop = if ended_torn { ReplayStop::TruncatedTail } else { ReplayStop::Clean };
    if !ranges.is_empty() {
        eprintln!(
            "kevy WARN: AOF {} resync skipped {} corrupt range(s) totalling {} bytes \
             and recovered the records after them; the bad bytes stay in place \
             until the next rewrite compacts them away.",
            path.display(),
            ranges.len(),
            ranges.iter().map(|(a, b)| b - a).sum::<u64>(),
        );
    }
    Ok(())
}

/// The resync walk over the damaged remainder (`rest` starts at absolute
/// offset `base`, positioned AT the first corrupt record). Hops to each
/// next valid record via [`crate::record::resync_scan`], applies the good
/// runs, and records every skipped `(abs_start, abs_end)` range. Returns
/// (local end of the last good record, records applied, ended-torn?).
fn resync_slice(
    rest: &[u8],
    base: u64,
    apply: &mut Option<&mut dyn FnMut(Argv)>,
    ranges: &mut Vec<(u64, u64)>,
) -> (u64, u64, bool) {
    let mut good_end = 0usize; // local offset after the last applied record
    let mut scan_from = 1usize; // the corrupt record starts at 0
    let mut applied = 0u64;
    loop {
        let Some(g) = crate::record::resync_scan(rest, scan_from) else {
            // Nothing valid ahead — the remainder past good_end is either
            // the corrupt region + torn tail or empty.
            return (good_end as u64, applied, true);
        };
        ranges.push((base + good_end as u64, base + g as u64));
        let mut w = g;
        loop {
            match crate::record::next_record(rest, w) {
                crate::record::RecordStep::Ok { payload, consumed } => {
                    match kevy_resp::parse_command(payload) {
                        Ok(Some((args, used))) if used == payload.len() => {
                            if let Some(f) = apply.as_deref_mut() {
                                f(args);
                            }
                            w += consumed;
                            applied += 1;
                            good_end = w;
                        }
                        _ => {
                            scan_from = w + 1;
                            break;
                        }
                    }
                }
                crate::record::RecordStep::Truncated => {
                    return (good_end as u64, applied, w < rest.len());
                }
                crate::record::RecordStep::Corrupt => {
                    scan_from = w + 1;
                    break;
                }
            }
        }
    }
}
