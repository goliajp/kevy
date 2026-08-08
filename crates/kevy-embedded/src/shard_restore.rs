//! Per-shard restore: segment directory, snapshot, AOF replay (with
//! the SEGMENTED stitch), orphan sweep. Split from `shard.rs` for the
//! 500-LOC house rule.

use std::io;
use std::path::Path;

use kevy_persist::{layout, load_snapshot, replay_aof};

use crate::config::Config;
use crate::metric::OpenReport;
use kevy_store::Store as Keyspace;

/// One shard's full restore: segment directory, snapshot, AOF replay,
/// orphan sweep, watermark drain.
pub(crate) fn restore_one_shard(
    dir: &Path,
    config: &Config,
    i: usize,
    store: &mut Keyspace,
    report: &mut OpenReport,
) -> io::Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    store
        .enable_seg_rows(&layout::segs_dir(dir, i))
        .map_err(io::Error::other)?;
    let snap = layout::snapshot_path(dir, i);
    if snap.exists() {
        load_snapshot(store, &snap)?;
    }
    let aof = layout::aof_path(dir, i);
    if aof.exists() {
        replay_shard_aof(dir, config, i, store, &aof, report)?;
    }
    #[cfg(not(target_arch = "wasm32"))]
    store.sweep_orphan_row_segs();
    store.demote_to_watermark();
    Ok(())
}

/// Replay one shard's AOF into its store, folding the outcome into
/// `report`. In-replay demotion: the embedded replay applies straight
/// to the bare store (no dispatch glue, so no per-write demote hook) —
/// check the watermark every K frames; the caller drains once more
/// after the log ends.
fn replay_shard_aof(
    dir: &Path,
    config: &Config,
    i: usize,
    store: &mut Keyspace,
    aof: &Path,
    report: &mut OpenReport,
) -> io::Result<()> {
    let _ = (dir, i);
    let mut frames = 0u64;
    #[cfg(not(target_arch = "wasm32"))]
    let segs_dir = layout::segs_dir(dir, i);
    #[cfg(not(target_arch = "wasm32"))]
    let mut torn: Option<String> = None;
    let apply = |args: kevy_persist::Argv| {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(f) = kevy_persist::segmented_frame(&args) {
            // The SEGMENTED stitch: re-do the hot-layer eviction; a
            // manifest miss is a named refusal after the walk (the
            // rows' durable copy is unreachable).
            if let Err(e) = kevy_store::apply_segmented(store, &segs_dir, f) {
                torn.get_or_insert(e);
            }
            return;
        }
        crate::replay::apply(store, &args);
        frames += 1;
        if frames.is_multiple_of(kevy_persist::REPLAY_DEMOTE_INTERVAL) {
            store.demote_to_watermark();
        }
    };
    // A registered metric sink receives the replay numbers as data
    // (`KevyMetric`), so the informational stderr summary would be a
    // duplicate on every open — a real cost for per-command CLI
    // processes. The corrupt-frame WARN prints regardless.
    let r = if config.metric_sink.is_some() {
        kevy_persist::replay_aof_quiet(aof, config.replay_resync, apply)?
    } else if config.replay_resync {
        kevy_persist::replay_aof_resync(aof, apply)?
    } else {
        replay_aof(aof, apply)?
    };
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(e) = torn {
        return Err(io::Error::other(format!("shard {i}: {e}")));
    }
    fold_replay_report(report, &r);
    Ok(())
}

/// Fold one shard's replay outcome into the open report.
fn fold_replay_report(report: &mut OpenReport, r: &kevy_persist::ReplayReport) {
    report.replayed_commands += r.commands;
    report.replayed_bytes += r.replayed_bytes;
    report.dropped_bytes += r.dropped_bytes;
    report.corrupt |= r.corrupt;
    report.resynced_bytes += r.resynced_ranges.iter().map(|(a, b)| b - a).sum::<u64>();
}
