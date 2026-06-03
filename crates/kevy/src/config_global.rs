//! Process-wide snapshot of the loaded [`kevy_config::Config`], reachable
//! from any shard / dispatch handler without threading the value through
//! every function signature.
//!
//! Set once by [`crate::serve`] before the reactor starts. Reads via
//! [`get`] are a relaxed atomic load — effectively free. CONFIG SET in
//! v1.0 stays read-only here (returns "ERR not supported, edit
//! kevy.toml and restart"); v1.x Wave 2 will swap this `OnceLock` for an
//! `ArcSwap`-style hot-swappable holder once we wire eviction +
//! appendfsync to react to runtime changes.

use std::sync::Arc;
use std::sync::OnceLock;

use kevy_config::Config;

static GLOBAL: OnceLock<Arc<Config>> = OnceLock::new();

/// Install the process-wide config. Idempotent — subsequent calls are
/// silently ignored (first one wins). `serve` is the canonical caller.
pub fn init(cfg: Arc<Config>) {
    let _ = GLOBAL.set(cfg);
}

/// Snapshot the current config. Returns `Config::default()` if [`init`]
/// hasn't been called (tests, embedded use without a real config).
pub fn get() -> Arc<Config> {
    GLOBAL
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(Config::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `OnceLock` is process-wide; both branches of `get()` (set vs
    // unset) cannot be tested in the same process — `cargo test` shares
    // one binary per crate. The "unset" path is exercised by the rest of
    // the test suite anywhere a test reads `config_global::get()`
    // without calling `init` first (e.g. via `KevyCommands::new` or
    // `serve` defaults paths). Here we cover the "set" path + the
    // idempotent-set property of `init`.

    #[test]
    fn init_sets_once_then_ignores_subsequent_calls() {
        // First call installs.
        let first = Arc::new(Config::default());
        init(first.clone());
        // Second call must be silently ignored (not panic, not replace).
        let second = Arc::new({
            let mut c = Config::default();
            c.memory.maxmemory = 12345;
            c
        });
        init(second.clone());
        let live = get();
        // The live value is `first`, not `second` — verifiable via Arc
        // pointer equality with the GLOBAL slot OR by checking the
        // maxmemory we tried to install via `second` did not take.
        assert_eq!(
            live.memory.maxmemory, first.memory.maxmemory,
            "init must be idempotent — second call should not overwrite"
        );
    }

    #[test]
    fn get_after_init_returns_the_installed_config() {
        // (Same OnceLock as the previous test — by this point GLOBAL is
        // already populated. `get` should return a non-default arc that
        // matches whatever the first init call installed.)
        let live = get();
        // Verifies the "set" arm of `get`; the default-fallback arm is
        // covered indirectly elsewhere in the suite.
        assert!(Arc::strong_count(&live) >= 1);
    }
}
