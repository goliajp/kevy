//! Process-wide catalogs owned by [`RuntimeState`].
//!
//! [`RuntimeState`]: crate::RuntimeState

use std::collections::HashMap;
use std::sync::Mutex;

pub(crate) struct CatalogState {
    /// Script cache shared across all shards: SCRIPT LOAD / EVAL write
    /// here, EVALSHA reads here and forwards the source to the
    /// per-shard `LuaHost` (so the per-shard VM pool still runs the
    /// script — thread-locality preserved). Cross-shard by design:
    /// a `SCRIPT LOAD` served on shard X must satisfy an `EVALSHA`
    /// routed to shard Y.
    pub(crate) scripts: Mutex<HashMap<[u8; 20], Vec<u8>>>,
}

impl CatalogState {
    pub(crate) fn new() -> Self {
        Self {
            scripts: Mutex::new(HashMap::new()),
        }
    }
}
