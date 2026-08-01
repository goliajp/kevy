//! Keyspace sharding for the embedded store: routing + per-shard persistence
//! bring-up (load / migrate). Each shard is a fully independent
//! `kevy_store::Store` + AOF behind its own lock (shared-nothing), so
//! concurrent access on different shards never contends. `n == 1` is the
//! original single-shard layout (one snapshot + one AOF under the configured
//! filenames, `dump-0.rdb` / `aof-0.aof` by default). `n > 1` keeps per-shard
//! `aof-{i}.aof` + a `shards.meta` recording the count; the first open at
//! `n > 1` re-shards a legacy single AOF into per-shard files.
//!
//! Dir interop with the `kevy` server: a single-shard dir is byte-identical
//! to the server's 1-thread layout, and `n == 1` records `shards.meta` too
//! so neither side needs inference.

use std::io;
#[cfg(feature = "persist")]
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
#[cfg(feature = "persist")]
use std::time::Instant;

use kevy_hash::KevyHash;
#[cfg(feature = "persist")]
use kevy_persist::reshard::{ShardLayout, commit_reshard, merge_sources, recover_journal};
#[cfg(feature = "persist")]
use kevy_persist::{
    Aof, Routing, ShardsMeta, layout, layout::infer_files_n, load_snapshot, read_shards_meta,
    replay_aof, write_shards_meta,
};
use kevy_store::Store as Keyspace;

use crate::config::{Config, TtlReaperMode};
#[cfg(feature = "persist")]
use crate::metric::KevyMetric;
use crate::metric::OpenReport;
use crate::store::Inner;

/// Route a key to its shard. `n == 1` short-circuits to 0; power-of-two `n`
/// uses a mask, else modulo. Same `KevyHash` the server's shard router uses.
#[inline]
pub(crate) fn shard_idx(key: &[u8], n: usize) -> usize {
    if n == 1 {
        return 0;
    }
    let h = key.kevy_hash() as usize;
    if n.is_power_of_two() {
        h & (n - 1)
    } else {
        h % n
    }
}

/// The embedded store's file layout for [`kevy_persist::reshard`]: the
/// standard per-shard names (`dump-{i}.rdb` / `aof-{i}.aof`). `n == 1`
/// coincides with shard 0's names, which are also the legacy single-file
/// names — old single-file dirs load without migration.
#[cfg(feature = "persist")]
struct EmbLayout;

#[cfg(feature = "persist")]
impl ShardLayout for EmbLayout {
    fn snapshot_path(&self, dir: &Path, i: usize, _n: usize) -> PathBuf {
        layout::snapshot_path(dir, i)
    }
    fn aof_path(&self, dir: &Path, i: usize, _n: usize) -> PathBuf {
        layout::aof_path(dir, i)
    }
}

fn fresh_keyspace(config: &Config) -> Keyspace {
    let mut s = Keyspace::new();
    s.set_max_memory(config.maxmemory, config.eviction_policy);
    s.set_cached_clock(matches!(config.ttl_reaper, TtlReaperMode::Background));
    s
}

/// Build the `n` shard `Inner`s for `config`, loading / migrating persistence.
/// The `bus` lives on shard 0 (pub/sub is process-wide, not sharded); other
/// shards get an idle bus that is never touched.
pub(crate) fn build_shards(config: &Config) -> io::Result<(Vec<Arc<RwLock<Inner>>>, OpenReport)> {
    let n = config.shards.max(1);
    #[allow(unused_mut)] // mut is the persist path's (load/reshard) need
    let mut stores: Vec<Keyspace> = (0..n).map(|_| fresh_keyspace(config)).collect();

    // Without the `persist` feature the build is always pure in-memory.
    #[cfg(not(feature = "persist"))]
    return Ok((into_inners_mem(stores), OpenReport::default()));

    #[cfg(feature = "persist")]
    build_shards_persist(config, n, stores)
}

/// Persistence bring-up half of [`build_shards`]: load / migrate the
/// on-disk layout, then open each shard's live AOF.
#[cfg(feature = "persist")]
fn build_shards_persist(
    config: &Config,
    n: usize,
    mut stores: Vec<Keyspace>,
) -> io::Result<(Vec<Arc<RwLock<Inner>>>, OpenReport)> {
    let Some(dir) = config.data_dir.clone() else {
        // Pure in-memory: no persistence, no AOF — and no disk for a
        // cold tier: tiering config on a mem-only store is a named
        // refusal, never a silent ignore.
        #[cfg(feature = "tier")]
        if config.tier_budget.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tiering requires a disk data dir (with_persist); a memory-only store has no cold tier",
            ));
        }
        return Ok((into_inners(stores, (0..n).map(|_| None).collect()), OpenReport::default()));
    };
    std::fs::create_dir_all(&dir)?;
    // Complete (or safely discard) a reshard a crash interrupted, before
    // reading the layout — same roll-forward the server runtime does.
    recover_journal(&dir, &EmbLayout)?;
    // Tiering comes up BEFORE the load: replay drives entries
    // into the hot map, so a dataset bigger than the budget must spill
    // inline as it replays — TierState has to exist by then. The vlog
    // wipe-at-open contract precedes the refill, so a reopen never
    // double-counts.
    enable_tiering(config, &dir, &mut stores)?;
    let mut report = load_or_reshard(&dir, config, n, &mut stores)?;

    let aofs = open_live_aofs(config, &dir, n, &mut report)?;
    Ok((into_inners(stores, aofs), report))
}

/// Open each shard's live AOF for append (if persistence is on). The
/// open repairs (quarantines + truncates) any dropped tail replay just
/// tolerated — the quarantine paths land in `report`.
#[cfg(feature = "persist")]
fn open_live_aofs(
    config: &Config,
    dir: &Path,
    n: usize,
    report: &mut OpenReport,
) -> io::Result<Vec<Option<Aof>>> {
    let aofs: Vec<Option<Aof>> = if config.aof {
        (0..n)
            .map(|i| {
                Aof::open_with_repair(
                    &layout::aof_path(dir, i),
                    config.appendfsync,
                    config.replay_resync,
                )
                .map(Some)
            })
            .collect::<io::Result<_>>()?
    } else {
        (0..n).map(|_| None).collect()
    };
    for aof in aofs.iter().flatten() {
        if let Some(q) = aof.open_quarantine() {
            report.quarantine_paths.push(q.to_path_buf());
        }
    }
    Ok(aofs)
}

/// Tiering bring-up: per-shard vlog at
/// `<dir>/tier/<i>`, opened (wiped — the vlog is per-boot disposable)
/// BEFORE replay, so in-replay demotion can spill a
/// bigger-than-budget dataset as it loads. The configured budget
/// resolves here (auto/percent probe the memory bound; both refuse by
/// name on failure) and is the whole store's — each shard gets an
/// even slice.
#[cfg(feature = "persist")]
fn enable_tiering(config: &Config, dir: &Path, stores: &mut [Keyspace]) -> io::Result<()> {
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    if config.tier_budget.is_some() {
        let per_shard = resolve_tier_budget(config, stores.len())?;
        for (i, store) in stores.iter_mut().enumerate() {
            store.enable_tiering(&dir.join("tier").join(i.to_string()), per_shard)?;
            store.set_tier_max_spill(config.max_spill_value);
        }
    }
    #[cfg(not(all(feature = "tier", not(target_arch = "wasm32"))))]
    let _ = (config, dir, stores);
    Ok(())
}

/// Resolve the configured tier budget spec to a PER-SHARD byte count
/// (whole-store budget / `nshards`, floored at 1). Named refusals:
/// percent out of 1..=100, auto/percent with no detectable bound.
#[cfg(all(feature = "persist", feature = "tier", not(target_arch = "wasm32")))]
pub(crate) fn resolve_tier_budget(config: &Config, nshards: usize) -> io::Result<u64> {
    let spec = config
        .tier_budget
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "tiering is not configured"))?;
    resolve_tier_spec(spec, nshards)
}

/// Spec-level half of [`resolve_tier_budget`] — also the reaper tick's
/// re-resolution entry (auto/percent re-probe the memory bound live).
#[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
pub(crate) fn resolve_tier_spec(
    spec: crate::config::TierBudgetSpec,
    nshards: usize,
) -> io::Result<u64> {
    use crate::config::TierBudgetSpec;
    if let TierBudgetSpec::Percent(p) = spec
        && !(1..=100).contains(&p)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("tiering budget percent must be 1..=100, got {p}"),
        ));
    }
    let total = spec.resolve().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "tiering budget auto/percent: no memory bound detected on this host — \
             use with_tier_budget(bytes)",
        )
    })?;
    Ok((total / nshards.max(1) as u64).max(1))
}

/// Per-tick tiering upkeep: re-resolve a probe-backed budget and
/// feed the index/view memory floor into the unified watermark. Runs
/// under the shard lock the tick already holds; one branch when
/// tiering is off.
#[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
pub(crate) fn tier_tick_upkeep(
    g: &mut crate::store::Inner,
    spec: Option<crate::config::TierBudgetSpec>,
    nshards: usize,
) {
    use crate::config::TierBudgetSpec;
    if !g.store.tier_enabled() {
        return;
    }
    if let Some(spec @ (TierBudgetSpec::Auto | TierBudgetSpec::Percent(_))) = spec
        && let Ok(per_shard) = resolve_tier_spec(spec, nshards)
    {
        g.store.set_tier_budget(per_shard);
    }
    #[cfg(feature = "index")]
    let reserved = g.idx_segs.reserved_bytes() + g.view_segs.reserved_bytes();
    #[cfg(not(feature = "index"))]
    let reserved = 0u64;
    g.store.set_tier_reserved(reserved);
}

/// Read the shard layout meta and either load in place (same layout)
/// or re-shard (losslessly) into the configured `n`.
#[cfg(feature = "persist")]
fn load_or_reshard(
    dir: &Path,
    config: &Config,
    n: usize,
    stores: &mut [Keyspace],
) -> io::Result<OpenReport> {
    let meta_path = layout::shards_meta_path(dir);
    let prev = read_shards_meta(&meta_path);
    // The embedded store always routes by KevyHash; a dir written by a
    // slots-routing server re-shards (losslessly) on first embedded open.
    let same_layout = match prev {
        Some(m) => m.n == n && m.routing == Routing::KevyHash,
        // No meta + n==1: the single-file layout — unless the files say
        // multi-shard (a meta-less pre-1.5 server dir). Loading only
        // shard 0 of those silently dropped (k-1)/k of the keyspace.
        None => n == 1 && infer_files_n(dir) <= 1,
    };

    if same_layout {
        let report = load_in_place(dir, config, n, stores)?;
        write_shards_meta(&meta_path, ShardsMeta { n, routing: Routing::KevyHash })?;
        return Ok(report);
    }
    {
        let src_n = prev.map(|m| m.n).or_else(|| {
            let k = infer_files_n(dir);
            (k > 1).then_some(k)
        });
        // The commit also records the new layout — including n == 1: a
        // stale meta from a larger prior n would otherwise trigger a second
        // re-shard next open, whose sources were already renamed to
        // `.premigration` (the shrink-to-one open would come up empty).
        reshard(dir, config, n, src_n, stores)
    }
}

/// Same-layout load: each shard reads its own snapshot + AOF directly.
#[cfg(feature = "persist")]
fn load_in_place(
    dir: &Path,
    config: &Config,
    _n: usize,
    stores: &mut [Keyspace],
) -> io::Result<OpenReport> {
    let mut report = OpenReport::default();
    let start = Instant::now();
    for (i, store) in stores.iter_mut().enumerate() {
        let snap = layout::snapshot_path(dir, i);
        if snap.exists() {
            load_snapshot(store, &snap)?;
        }
        let aof = layout::aof_path(dir, i);
        if aof.exists() {
            replay_shard_aof(dir, config, i, store, &aof, &mut report)?;
        }
        store.demote_to_watermark();
    }
    report.elapsed_ms = start.elapsed().as_millis() as u64;
    emit_replay(config, &report);
    Ok(report)
}

/// Replay one shard's AOF into its store, folding the outcome into
/// `report`. In-replay demotion: the embedded replay applies straight
/// to the bare store (no dispatch glue, so no per-write demote hook) —
/// check the watermark every K frames; the caller drains once more
/// after the log ends.
#[cfg(feature = "persist")]
fn replay_shard_aof(
    dir: &Path,
    config: &Config,
    i: usize,
    store: &mut Keyspace,
    aof: &Path,
    report: &mut OpenReport,
) -> io::Result<()> {
    let _ = dir;
    let mut frames = 0u64;
    #[cfg(not(target_arch = "wasm32"))]
    let segs_dir = layout::segs_dir(dir, i);
    #[cfg(not(target_arch = "wasm32"))]
    store.enable_seg_rows(&segs_dir).map_err(io::Error::other)?;
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
    let r = if config.replay_resync {
        kevy_persist::replay_aof_resync(aof, apply)?
    } else {
        replay_aof(aof, apply)?
    };
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(e) = torn {
        return Err(io::Error::other(format!("shard {i}: {e}")));
    }
    #[cfg(not(target_arch = "wasm32"))]
    store.sweep_orphan_row_segs();
    fold_replay_report(report, &r);
    Ok(())
}

/// Fold one shard's replay outcome into the open report.
#[cfg(feature = "persist")]
fn fold_replay_report(report: &mut OpenReport, r: &kevy_persist::ReplayReport) {
    report.replayed_commands += r.commands;
    report.replayed_bytes += r.replayed_bytes;
    report.dropped_bytes += r.dropped_bytes;
    report.corrupt |= r.corrupt;
    report.resynced_bytes += r.resynced_ranges.iter().map(|(a, b)| b - a).sum::<u64>();
}

/// Re-shard: load every source file into one temp keyspace, redistribute
/// each key to its target shard, then hand the crash-safe commit to
/// [`kevy_persist::reshard`] — per-shard snapshots land under `.reshard`
/// temp names, a durable journal marks the commit point, and only then are
/// the sources backed up (`.premigration.<nanos>`) and the temps finalized.
/// A crash at any point either leaves the old layout intact or is rolled
/// forward by `build_shards`' recovery on the next open. Each shard's fresh
/// AOF opens after this returns; the snapshot is the full migrated state.
#[cfg(feature = "persist")]
fn reshard(
    dir: &Path,
    config: &Config,
    n: usize,
    prev_n: Option<usize>,
    stores: &mut [Keyspace],
) -> io::Result<OpenReport> {
    let lay = EmbLayout;
    // Source layout: prior shard files, or a legacy single AOF/snapshot.
    let src_n = prev_n.unwrap_or(1);
    let (temp, report) = merge_into_temp(dir, config, src_n)?;
    redistribute(&temp, n, stores);
    commit_reshard(dir, src_n, ShardsMeta { n, routing: Routing::KevyHash }, stores, &lay)?;
    // The merge scratch vlog is dead once the temp keyspace is gone.
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    if config.tier_budget.is_some() {
        drop(temp);
        let _ = std::fs::remove_dir_all(dir.join("tier").join(".reshard-merge"));
    }
    Ok(report)
}

/// Merge every source file into one temp keyspace and report the replay
/// metrics. B11 on the migration path: a merged dataset bigger than the
/// tier budget must not OOM either — the temp keyspace tiers into a
/// scratch vlog dir (wiped by `reshard` after redistribution) and the
/// merge demotes inline exactly like boot replay does.
#[cfg(feature = "persist")]
fn merge_into_temp(dir: &Path, config: &Config, src_n: usize) -> io::Result<(Keyspace, OpenReport)> {
    let lay = EmbLayout;
    let mut temp = fresh_keyspace(config);
    #[cfg(all(feature = "tier", not(target_arch = "wasm32")))]
    if config.tier_budget.is_some() {
        // The merge temp holds the whole keyspace — full store budget.
        let budget = resolve_tier_budget(config, 1)?;
        temp.enable_tiering(&dir.join("tier").join(".reshard-merge"), budget)?;
        temp.set_tier_max_spill(config.max_spill_value);
    }
    let mut total_cmds = 0u64;
    let start = Instant::now();
    merge_sources(dir, src_n, &lay, &mut temp, |store, args| {
        total_cmds += 1;
        crate::replay::apply(store, &args);
        if total_cmds.is_multiple_of(kevy_persist::REPLAY_DEMOTE_INTERVAL) {
            store.demote_to_watermark();
        }
    })?;
    // Replay-metric byte count: the source AOFs (sizes read before the
    // commit renames them away). Per-file drop/corrupt detail is not
    // plumbed through the merge path — a migrating open reports totals.
    let total_bytes = (0..src_n)
        .map(|i| lay.aof_path(dir, i, src_n))
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let report = OpenReport {
        replayed_commands: total_cmds,
        replayed_bytes: total_bytes,
        elapsed_ms: start.elapsed().as_millis() as u64,
        ..OpenReport::default()
    };
    emit_replay(config, &report);
    Ok((temp, report))
}

/// Redistribute the merged keyspace into the target shards. A cold
/// stub names the TEMP keyspace's vlog — a foreign log the target
/// cannot read — so the source side materializes before shipping
/// (`load_value`'s Cold arm is unreachable by this contract); the
/// target demotes inline to stay under its own budget (B11).
#[cfg(feature = "persist")]
fn redistribute(temp: &Keyspace, n: usize, stores: &mut [Keyspace]) {
    temp.snapshot_each(|key, value, ttl_ms| {
        let hot;
        let value = match temp.materialize_cold(key, value) {
            Some(v) => {
                hot = v;
                &hot
            }
            None => value,
        };
        let target = &mut stores[shard_idx(key, n)];
        target.load_value(key, value, ttl_ms);
        target.try_demote_after_write();
    });
}

#[cfg(feature = "persist")]
fn emit_replay(config: &Config, report: &OpenReport) {
    if let Some(sink) = &config.metric_sink {
        sink.emit(KevyMetric::Replay {
            commands: report.replayed_commands,
            bytes: report.replayed_bytes + report.dropped_bytes,
            elapsed_ms: report.elapsed_ms,
            dropped_bytes: report.dropped_bytes,
            corrupt: report.corrupt,
        });
    }
}

#[cfg(feature = "persist")]
fn into_inners(stores: Vec<Keyspace>, aofs: Vec<Option<Aof>>) -> Vec<Arc<RwLock<Inner>>> {
    stores
        .into_iter()
        .zip(aofs)
        .map(|(store, aof)| Arc::new(RwLock::new(Inner::new(store, aof))))
        .collect()
}

/// In-memory-only `Inner` build (the whole story without `persist`).
#[cfg(not(feature = "persist"))]
fn into_inners_mem(stores: Vec<Keyspace>) -> Vec<Arc<RwLock<Inner>>> {
    stores
        .into_iter()
        .map(|store| Arc::new(RwLock::new(Inner::new(store))))
        .collect()
}
