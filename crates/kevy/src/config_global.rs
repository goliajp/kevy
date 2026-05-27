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
