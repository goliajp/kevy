//! Boot-time wiring of `[tiering]`: the configured budget resolved to
//! bytes and handed to the runtime.

use crate::KevyCommands;

/// Tiering: resolve the `[tiering]` budget to bytes — auto/percent
/// probe the OS bound via kevy-sys — and hand the runtime the
/// process-level number (it splits per shard). A spec that cannot
/// resolve is a named boot refusal, never a silent off.
pub(crate) fn wire_tiering(
    runtime: kevy_rt::Runtime<KevyCommands>,
    cfg: &kevy_config::Config,
) -> kevy_rt::Runtime<KevyCommands> {
    match resolve_tier_budget(cfg) {
        Ok(budget) => {
            runtime.with_tier_budget(budget).with_tier_spill_dir(cfg.tiering.spill_dir.clone())
        }
        Err(msg) => {
            eprintln!("kevy: {msg}");
            std::process::exit(1);
        }
    }
}

/// Resolve the configured `[tiering] budget` to bytes. `Ok(None)` =
/// tiering off; `Err` = an auto/percent form with no detectable memory
/// bound (named refusal at boot; on the tick the caller keeps the last
/// resolved value instead).
pub(crate) fn resolve_tier_budget(cfg: &kevy_config::Config) -> Result<Option<u64>, String> {
    match cfg.tiering.budget {
        None => Ok(None),
        Some(spec) => {
            spec.resolve_with(kevy_sys::detected_memory_bound()).map(Some).ok_or_else(|| {
                format!(
                    "[tiering] budget = \"{}\": no memory bound detected on this host \
                     (cgroup v2 memory.max / /proc/meminfo MemAvailable / hw.memsize all \
                     unavailable) — use an absolute budget (\"4gb\")",
                    spec.to_config_string()
                )
            })
        }
    }
}
