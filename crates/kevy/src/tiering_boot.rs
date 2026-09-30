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
        Some(spec) => resolve_spec(spec, kevy_sys::detected_memory_bound()).map(Some),
    }
}

/// `spec` in bytes against the host's memory bound, or the refusal that
/// names the setting when an auto/percent form has no bound to take from.
fn resolve_spec(spec: kevy_config::TierBudgetSpec, bound: Option<u64>) -> Result<u64, String> {
    spec.resolve_with(bound).ok_or_else(|| {
        format!(
            "[tiering] budget = \"{}\": no memory bound detected on this host \
             (cgroup v2 memory.max / /proc/meminfo MemAvailable / hw.memsize all \
             unavailable) — use an absolute budget (\"4gb\")",
            spec.to_config_string()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_config::TierBudgetSpec;

    #[test]
    fn an_absolute_budget_needs_no_bound_and_a_relative_one_refuses_without_it() {
        assert_eq!(resolve_spec(TierBudgetSpec::parse("4mb").unwrap(), None), Ok(4 << 20));
        assert_eq!(resolve_spec(TierBudgetSpec::parse("50%").unwrap(), Some(1000)), Ok(500));
        let err = resolve_spec(TierBudgetSpec::parse("50%").unwrap(), None).unwrap_err();
        assert!(err.starts_with("[tiering] budget = \"50%\": no memory bound detected"), "{err}");
        assert!(err.ends_with("use an absolute budget (\"4gb\")"), "{err}");
    }

    #[test]
    fn an_untiered_config_resolves_to_no_budget() {
        let mut cfg = kevy_config::Config::default();
        assert_eq!(resolve_tier_budget(&cfg), Ok(None));
        cfg.tiering.budget = Some(TierBudgetSpec::parse("1mb").unwrap());
        assert_eq!(resolve_tier_budget(&cfg), Ok(Some(1 << 20)));
    }
}
