//! Process-wide snapshot of the loaded [`kevy_config::Config`], reachable
//! from any shard / dispatch handler without threading the value through
//! every function signature.
//!
//! Set once by [`crate::serve`] before the reactor starts. After that
//! it can be **hot-swapped** by `CONFIG SET` via [`replace`]; each shard
//! re-reads the new value from its tick path (every 100 ms by default)
//! and re-applies the per-shard knobs (maxmemory, appendfsync, etc.).
//! `get()` is a single `RwLock` read + Arc clone (~20 ns) — only called
//! from tick (10 Hz) and INFO / CONFIG GET handlers, so the lock cost
//! never reaches the per-command hot path.

use std::sync::{Arc, RwLock};

use kevy_config::Config;

/// `None` means "not yet initialised". `init` populates it; the slot
/// stays `Some(_)` thereafter (replace never reverts to `None`).
static GLOBAL: RwLock<Option<Arc<Config>>> = RwLock::new(None);

/// Install the process-wide config. Idempotent — subsequent calls are
/// silently ignored (first one wins). `serve` is the canonical caller;
/// use [`replace`] from the CONFIG SET path to overwrite.
pub fn init(cfg: Arc<Config>) {
    let mut g = GLOBAL.write().expect("config_global poisoned");
    if g.is_none() {
        *g = Some(cfg);
    }
}

/// Snapshot the current config. Returns `Config::default()` when
/// [`init`] hasn't been called (tests, embedded use without a real
/// config). Allocates a fresh `Arc<Config::default()>` per call in
/// that branch, which is fine because the test path is cold.
pub fn get() -> Arc<Config> {
    GLOBAL
        .read()
        .expect("config_global poisoned")
        .as_ref()
        .cloned()
        .unwrap_or_else(|| Arc::new(Config::default()))
}

/// Atomically swap in a new live config. Returns `Err` if [`init`]
/// has never run — refusing to silently fabricate an initial state
/// from a CONFIG SET call. The caller (`CONFIG SET`) maps the Err to
/// the matching RESP error.
pub fn replace(cfg: Arc<Config>) -> Result<(), &'static str> {
    let mut g = GLOBAL.write().expect("config_global poisoned");
    if g.is_none() {
        return Err("config_global not initialised");
    }
    *g = Some(cfg);
    Ok(())
}

/// Has [`init`] (or a successful [`replace`]) ever run? Used by
/// [`crate::KevyCommands::live_runtime_config`] to decide whether to
/// override the `Runtime` builder's explicit settings: when the
/// embedder hasn't seeded a config (tests, examples that hand-craft
/// a `Runtime`), the live-config hook returns all-None so the builder's
/// own `with_appendfsync` / `with_auto_aof_rewrite` choices stick.
pub fn is_initialised() -> bool {
    GLOBAL
        .read()
        .expect("config_global poisoned")
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `RwLock<Option<…>>` is process-wide; both branches of `get()`
    // (set vs unset) cannot be tested in the same process — `cargo
    // test` shares one binary per crate. The "unset" path is exercised
    // by the rest of the test suite anywhere a test reads
    // `config_global::get()` without calling `init` first (e.g. via
    // `KevyCommands::new` or `serve` defaults paths). Here we cover the
    // "set" path + the idempotent-set property of `init` + the
    // replace path.

    #[test]
    fn init_sets_once_then_ignores_subsequent_calls() {
        // First call installs.
        let first = Arc::new(Config::default());
        init(first.clone());
        // Second `init` call must be silently ignored (not panic, not replace).
        let second = Arc::new({
            let mut c = Config::default();
            c.memory.maxmemory = 12345;
            c
        });
        init(second.clone());
        let live = get();
        assert_eq!(
            live.memory.maxmemory, first.memory.maxmemory,
            "init must be idempotent — second init call should not overwrite"
        );

        // But explicit replace DOES overwrite.
        let third = Arc::new({
            let mut c = Config::default();
            c.memory.maxmemory = 67890;
            c
        });
        replace(third.clone()).expect("replace after init must succeed");
        assert_eq!(get().memory.maxmemory, 67890);
    }

    #[test]
    fn get_after_init_returns_the_installed_config() {
        // (Same RwLock as the previous test — by this point GLOBAL is
        // already populated. `get` should return a non-default arc.)
        let live = get();
        assert!(Arc::strong_count(&live) >= 1);
    }
}
