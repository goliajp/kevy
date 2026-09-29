//! Per-shard restore: segment directory, snapshot, AOF replay (with
//! the SEGMENTED stitch), then what the staging ring owes. Split from `shard.rs` for the
//! 500-LOC house rule.

use std::io;
use std::path::Path;

use kevy_persist::{layout, load_snapshot};

use crate::config::Config;
use crate::metric::OpenReport;
use kevy_store::Store as Keyspace;

/// One shard's restore from its files: segment directory, snapshot, AOF
/// replay, watermark drain. The orphan sweep waits for the staging ring,
/// which may still owe a SEGMENTED frame. Returns where the replay stopped when it
/// dropped nothing (see [`kevy_persist::Aof::open_after_replay`]).
pub(crate) fn restore_one_shard(
    dir: &Path,
    config: &Config,
    i: usize,
    store: &mut Keyspace,
    report: &mut OpenReport,
) -> io::Result<Option<u64>> {
    #[cfg(not(target_arch = "wasm32"))]
    store.enable_seg_rows(&layout::segs_dir(dir, i)).map_err(io::Error::other)?;
    let snap = layout::snapshot_path(dir, i);
    if snap.exists() {
        load_snapshot(store, &snap)?;
    }
    let aof = layout::aof_path(dir, i);
    let mut whole = None;
    if aof.exists() {
        whole = replay_shard_aof(dir, config, i, store, &aof, report)?;
    }
    store.demote_to_watermark();
    Ok(whole)
}

/// Applies logged frames to one shard's keyspace, for the AOF replay and
/// for what a staging ring owes it: the SEGMENTED stitch, every other
/// write through the shared command layer, and the tiering watermark every
/// so many frames — the embedded replay applies straight to the bare store,
/// with no dispatch glue to run the per-write demote hook.
struct FrameApplier<'a> {
    store: &'a mut Keyspace,
    #[cfg(not(target_arch = "wasm32"))]
    segs_dir: std::path::PathBuf,
    #[cfg(not(target_arch = "wasm32"))]
    torn: Option<String>,
    frames: u64,
}

impl<'a> FrameApplier<'a> {
    fn new(dir: &Path, i: usize, store: &'a mut Keyspace) -> Self {
        let _ = (dir, i);
        FrameApplier {
            store,
            #[cfg(not(target_arch = "wasm32"))]
            segs_dir: layout::segs_dir(dir, i),
            #[cfg(not(target_arch = "wasm32"))]
            torn: None,
            frames: 0,
        }
    }

    fn apply(&mut self, args: &mut kevy_persist::Argv) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(f) = kevy_persist::segmented_frame(args) {
            // The SEGMENTED stitch: re-do the hot-layer eviction; a
            // manifest miss is a named refusal after the walk (the
            // rows' durable copy is unreachable).
            if let Err(e) = self.store.apply_segmented(&self.segs_dir, f) {
                self.torn.get_or_insert(e);
            }
            return;
        }
        crate::replay::apply(self.store, args);
        self.frames += 1;
        if self.frames.is_multiple_of(kevy_persist::REPLAY_DEMOTE_INTERVAL) {
            self.store.demote_to_watermark();
        }
    }

    fn finish(self, i: usize) -> io::Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(e) = self.torn {
            return Err(io::Error::other(format!("shard {i}: {e}")));
        }
        let _ = i;
        Ok(())
    }
}

/// Replay one shard's AOF into its store, folding the outcome into
/// `report`; the caller drains the demote watermark once more after the
/// log ends.
fn replay_shard_aof(
    dir: &Path,
    config: &Config,
    i: usize,
    store: &mut Keyspace,
    aof: &Path,
    report: &mut OpenReport,
) -> io::Result<Option<u64>> {
    let mut applier = FrameApplier::new(dir, i, store);
    // A registered metric sink receives the replay numbers as data
    // (`KevyMetric`), so the informational stderr summary would be a
    // duplicate on every open — a real cost for per-command CLI
    // processes. The corrupt-frame WARN prints regardless.
    let summary = if config.metric_sink.is_some() {
        kevy_persist::ReplaySummary::Quiet
    } else {
        kevy_persist::ReplaySummary::Print
    };
    let r = kevy_persist::replay_aof_in_place(aof, config.replay_mode(), summary, |a| {
        applier.apply(a);
    })?;
    applier.finish(i)?;
    fold_replay_report(report, &r);
    Ok((r.dropped_bytes == 0).then_some(r.replayed_bytes))
}

/// Set how shard `i`'s freshly opened AOF takes appends: through a mapping,
/// through a staging ring, or straight through `write()`. First, whatever
/// a ring the last process left owes the log is replayed into `store`;
/// a log that does not stage then removes that ring.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn open_stage(
    dir: &Path,
    config: &Config,
    i: usize,
    store: &mut Keyspace,
    aof: &mut kevy_persist::Aof,
    report: &mut OpenReport,
) -> io::Result<()> {
    let path = layout::stage_path(dir, i);
    let always = config.appendfsync == crate::config::AppendFsync::Always;
    let maps = config.mapped_aof && !always;
    let stages = !maps && config.stage_bytes > 0 && !always;
    let mut applier = FrameApplier::new(dir, i, store);
    let found = if stages {
        aof.open_stage(&path, config.stage_bytes, |a| applier.apply(a))?
    } else {
        aof.settle_stage(&path, |a| applier.apply(a))?
    };
    applier.finish(i)?;
    store.demote_to_watermark();
    report.stage_recovered += found.recovered;
    report.stage_discarded += u64::from(found.discarded.is_some());
    if maps {
        aof.map_appends()?;
    }
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
