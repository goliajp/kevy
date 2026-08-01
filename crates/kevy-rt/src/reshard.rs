//! Server-side shard-layout bring-up: detect a `shards.meta` mismatch and
//! re-home every key before any shard thread spawns.
//!
//! The crash-safe mechanics (temp snapshots → journal commit point →
//! finalize, with roll-forward recovery on the next start) live in
//! [`kevy_persist::reshard`] — shared with the embedded store. This file
//! only wires the server's specifics in: AOF frames replay through the full
//! command table, and keys redistribute under the server's routing
//! (KevyHash, or slot ranges in cluster mode).

use crate::Commands;
use crate::reduce::shard_of;
use kevy_persist::reshard::{StdLayout, commit_reshard, merge_sources, recover_journal};
use kevy_persist::{Routing, ShardsMeta, layout, read_shards_meta, write_shards_meta};
use kevy_store::Store;
use std::io;
use std::path::Path;

/// Ensure `dir`'s persisted layout matches `(n, routing)`, re-sharding once
/// if it doesn't. Called by `Runtime::run` before any shard thread spawns;
/// afterwards each shard loads its own files exactly as before. A reshard
/// interrupted by a crash is completed (or safely discarded) first.
pub(crate) fn ensure_layout<C: Commands>(
    dir: &Path,
    n: usize,
    routing: Routing,
    commands: &C,
    tier_budget: Option<u64>,
    tier_root: &Path,
) -> io::Result<()> {
    let meta_path = layout::shards_meta_path(dir);
    recover_journal(dir, &StdLayout)?;
    let target = ShardsMeta { n, routing };
    let prev = match read_shards_meta(&meta_path) {
        Some(m) => m,
        // Legacy dir (server never wrote meta): the shard count is however
        // many per-shard files exist, the routing is the only scheme that
        // existed. An empty dir trivially "matches" — just record target.
        None => ShardsMeta {
            n: layout::infer_files_n(dir),
            routing: Routing::KevyHash,
        },
    };
    if prev.n == 0 || prev == target {
        std::fs::create_dir_all(dir)?;
        return write_shards_meta(&meta_path, target);
    }
    reshard(dir, prev, target, commands, tier_budget, tier_root)
}

/// Whether `dir` holds any kevy persistence artifacts (per-shard snapshot,
/// AOF, or a `shards.meta`). Gates layout reconciliation for pure in-memory
/// runs so they keep writing nothing.
pub(crate) fn has_kevy_files(dir: &Path) -> bool {
    layout::infer_files_n(dir) > 0 || layout::shards_meta_path(dir).exists()
}

/// Merge every `prev` source file into one temp store (AOF frames replayed
/// through the command table), redistribute under `target`'s routing, then
/// hand the crash-safe commit to the engine — which also records the new
/// layout in `shards.meta`.
///
/// Tiering: under the runtime's tiering budget (the resolved
/// builder value, or the minimal `KEVY_TIER_BUDGET` env knob), the temp
/// store and the redistribution targets tier into scratch vlog dirs so
/// a merged dataset bigger than the budget migrates without OOM. Cold
/// stubs materialize on the SOURCE side before shipping (a stub names
/// the source's vlog, foreign to the target); the committed snapshots
/// materialize again through the `SnapshotSource` contract. The scratch
/// dirs are removed after the commit — the per-shard boots re-open (and
/// wipe) their real `<tier root>/<id>` logs afterwards. The temp store
/// gets the full process budget; targets get their per-shard slice.
fn reshard<C: Commands>(
    dir: &Path,
    prev: ShardsMeta,
    target: ShardsMeta,
    commands: &C,
    tier_budget: Option<u64>,
    tier_root: &Path,
) -> io::Result<()> {
    let scratch = |name: String| tier_root.join(name);
    let mut temp = Store::new();
    if let Some(budget) = tier_budget {
        temp.enable_tiering(&scratch(".reshard-merge".into()), budget)?;
    }
    let mut frames: u64 = 0;
    let sources = merge_sources(dir, prev.n, &StdLayout, &mut temp, |store, args| {
        crate::shard_run::replay_dispatch(commands, store, &args);
        frames += 1;
        if frames.is_multiple_of(kevy_persist::REPLAY_DEMOTE_INTERVAL) {
            store.demote_to_watermark();
        }
    })?;

    let mut stores: Vec<Store> = (0..target.n).map(|_| Store::new()).collect();
    if let Some(budget) = tier_budget {
        let per = crate::Runtime::<C>::per_shard_tier_budget(budget, target.n);
        for (i, s) in stores.iter_mut().enumerate() {
            s.enable_tiering(&scratch(format!(".reshard-{i}")), per)?;
        }
    }
    redistribute(&temp, target, &mut stores);

    let stamp = commit_reshard(dir, prev.n, target, &stores, &StdLayout)?;
    if tier_budget.is_some() {
        drop(temp);
        drop(stores);
        let _ = std::fs::remove_dir_all(scratch(".reshard-merge".into()));
        for i in 0..target.n {
            let _ = std::fs::remove_dir_all(scratch(format!(".reshard-{i}")));
        }
    }
    eprintln!(
        "kevy: re-sharded {} -> {} shards ({:?} -> {:?} routing); {} source file(s) backed up as .premigration.{stamp}",
        prev.n, target.n, prev.routing, target.routing, sources.len(),
    );
    Ok(())
}

/// Re-home every merged key under the target routing. A cold stub names
/// the TEMP store's vlog — a foreign log the target cannot read — so
/// the source side materializes before shipping (`load_value`'s Cold
/// arm is unreachable by this contract); the target demotes inline to
/// stay under its own budget (B11).
fn redistribute(temp: &Store, target: ShardsMeta, stores: &mut [Store]) {
    let slots = target.routing == Routing::Slots;
    temp.snapshot_each(|key, value, ttl_ms| {
        let hot;
        let value = match temp.materialize_cold(key, value) {
            Some(v) => {
                hot = v;
                &hot
            }
            None => value,
        };
        let t = &mut stores[shard_of(key, target.n, slots)];
        t.load_value(key, value, ttl_ms);
        t.try_demote_after_write();
    });
}
