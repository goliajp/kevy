//! Process-level runtime state (K-103).
//!
//! [`RuntimeState`] is the single owner of every cross-shard piece of
//! server state that used to live in process-wide statics: the live
//! config, the scope-routing tables, the election plane, the shared
//! catalogs and the observability slots. [`KevyCommands`] carries an
//! `Arc<RuntimeState>` into every shard (the `kevy-rt` runtime clones
//! its `Commands` value once per shard), plus a per-shard private
//! [`ShardCtx`] that is deliberately **not** copied by `Clone` — each
//! clone starts with a fresh shard identity.
//!
//! Dispatch handlers that need global state receive a [`Ctx`] — a
//! borrow of both halves — threaded down from the `Commands` trait
//! impl in [`crate::commands`].

mod catalogs;
mod election;
mod obs;
mod scope;

pub(crate) use catalogs::CatalogState;
pub(crate) use election::ElectionState;
pub(crate) use obs::{ObsState, ShardStats, Totals};
pub(crate) use scope::{ScopeState, WriteRedirect, encode_misdirected, encode_quiesced};

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use kevy_config::Config;

/// Everything the server knows that is not per-shard keyspace data.
/// Built once (by [`crate::serve`] or an embedder) and shared across
/// shards behind an `Arc`.
pub struct RuntimeState {
    /// The live config. Hot-swapped by `CONFIG SET` via
    /// [`Self::config_replace`]; each shard re-reads the new value
    /// from its tick path (every 100 ms by default) and re-applies the
    /// per-shard knobs (maxmemory, appendfsync, …). A read is a single
    /// `RwLock` read + Arc clone (~20 ns) — only paid on tick and in
    /// INFO / CONFIG-class handlers, never on the per-command hot path.
    config: RwLock<Arc<Config>>,
    /// Whether an operator/embedder-provided config has ever been
    /// installed (constructor with an explicit config, or a
    /// `config_replace`). [`crate::KevyCommands::new`] starts with a
    /// `Config::default()` fallback and this flag `false`, which makes
    /// `live_runtime_config` return all-`None` so a hand-built
    /// `Runtime`'s explicit builder choices aren't clobbered by
    /// default values.
    config_explicit: AtomicBool,
    /// Sidecar/persistence root (index + view catalogs, elect.meta).
    /// Empty = no sidecar persistence (embedded / test use).
    data_dir: PathBuf,
    /// Shard count this state was sized for (stats slots, election
    /// offset slots). Must match the runtime it is plugged into.
    nshards: usize,
    pub(crate) election: ElectionState,
    pub(crate) scope: ScopeState,
    pub(crate) catalogs: CatalogState,
    pub(crate) obs: ObsState,
}

impl RuntimeState {
    /// Build the state for an explicit config. `data_dir` is the
    /// sidecar/persistence root (pass an empty path to disable sidecar
    /// persistence); `nshards` must match the runtime this state will
    /// serve. Returns `Err(msg)` when `[cluster] scopes` fails the
    /// linter — bad scope config fails at construction, not at the
    /// first wrong-shard write.
    pub fn new(cfg: Arc<Config>, data_dir: PathBuf, nshards: usize) -> Result<Self, String> {
        let mut state = Self::build(cfg, data_dir, nshards)?;
        *state.config_explicit.get_mut() = true;
        Ok(state)
    }

    /// Default-config state for embedded / test use: no data dir, no
    /// sidecar persistence, and `live_runtime_config` stays all-`None`
    /// until a real config is installed via [`Self::config_replace`].
    fn embedded(nshards: usize) -> Self {
        Self::build(Arc::new(Config::default()), PathBuf::new(), nshards)
            .expect("Config::default() declares no scopes")
    }

    fn build(cfg: Arc<Config>, data_dir: PathBuf, nshards: usize) -> Result<Self, String> {
        Ok(Self {
            scope: ScopeState::from_config(&cfg)?,
            election: ElectionState::new(nshards),
            catalogs: CatalogState::new(),
            obs: ObsState::new(&cfg.audit.log_path, nshards),
            config: RwLock::new(cfg),
            config_explicit: AtomicBool::new(false),
            data_dir,
            nshards,
        })
    }

    /// Snapshot the current config.
    pub fn config(&self) -> Arc<Config> {
        self.config.read().expect("config poisoned").clone()
    }

    /// Atomically swap in a new live config (the `CONFIG SET` path,
    /// also available to embedders for hot reload).
    pub fn config_replace(&self, cfg: Arc<Config>) {
        *self.config.write().expect("config poisoned") = cfg;
        self.config_explicit.store(true, Ordering::Release);
    }

    /// Has an explicit config ever been installed? See the field doc.
    pub(crate) fn config_is_explicit(&self) -> bool {
        self.config_explicit.load(Ordering::Acquire)
    }

    /// Data dir for the index/view catalog sidecars. `None` when this
    /// state was built without one (embedded / test use) — catalog
    /// mutations then skip persistence, matching the pre-4.0 embedded
    /// behaviour.
    pub(crate) fn sidecar_dir(&self) -> Option<&Path> {
        if self.data_dir.as_os_str().is_empty() {
            None
        } else {
            Some(&self.data_dir)
        }
    }

    pub(crate) fn nshards(&self) -> usize {
        self.nshards
    }
}

/// Per-shard private zone. Lives inside [`KevyCommands`]; the manual
/// `Clone` impl gives every clone a **fresh** `ShardCtx`, so nothing
/// here ever leaks across shards. Interior mutability is `Cell` /
/// `RefCell` — a shard is single-threaded by construction
/// (thread-per-core), so no atomics are needed.
#[derive(Default)]
pub(crate) struct ShardCtx {
    /// This shard's id, written by `Commands::on_shard_start`. `None`
    /// outside a reactor thread (embedded / tests / the runtime main
    /// thread) — readers fall back to shard 0, the same convention the
    /// pre-4.0 thread-locals used.
    shard_id: Cell<Option<usize>>,
    /// Set for the duration of a `MOVE-SCOPE-INGEST` dispatch.
    /// `RuntimeState::route_write` checks this FIRST and treats a
    /// matching key as locally writable — the target node accepts the
    /// source's reconstruction commands without bouncing them back via
    /// `-MISDIRECTED`.
    ingesting_prefix: RefCell<Option<Vec<u8>>>,
}

impl ShardCtx {
    pub(crate) fn set_shard_id(&self, shard: usize) {
        self.shard_id.set(Some(shard));
    }

    /// This shard's id, defaulting to 0 outside a reactor thread.
    pub(crate) fn shard_id(&self) -> usize {
        self.shard_id.get().unwrap_or(0)
    }

    /// RAII guard for the MOVE-SCOPE-INGEST window. Cleared on drop;
    /// nested guards aren't expected (a recursion into another
    /// MOVE-SCOPE-INGEST inside one would be a bug — the inner ingest
    /// silently inherits the outer's prefix until both drop).
    pub(crate) fn ingest_guard(&self, prefix: Vec<u8>) -> IngestGuard<'_> {
        *self.ingesting_prefix.borrow_mut() = Some(prefix);
        IngestGuard { shard: self }
    }

    pub(crate) fn ingesting_matches(&self, key: &[u8]) -> bool {
        self.ingesting_prefix
            .borrow()
            .as_ref()
            .is_some_and(|p| key.starts_with(p))
    }
}

/// See [`ShardCtx::ingest_guard`].
pub(crate) struct IngestGuard<'a> {
    shard: &'a ShardCtx,
}

impl Drop for IngestGuard<'_> {
    fn drop(&mut self) {
        *self.shard.ingesting_prefix.borrow_mut() = None;
    }
}

/// Borrowed view of both state halves, threaded through the dispatch
/// tree to every handler that touches process-level state. Pure data
/// handlers (string / hash / list / …) never see it.
pub(crate) struct Ctx<'a> {
    /// `&Arc` (not `&RuntimeState`) so long-lived captures — e.g. the
    /// per-shard `LuaHost` dispatch closure — can clone the Arc.
    pub(crate) state: &'a Arc<RuntimeState>,
    pub(crate) shard: &'a ShardCtx,
}

/// kevy's command set, plugged into the `kevy-rt` runtime: the shared
/// [`RuntimeState`] plus this shard's private [`ShardCtx`]. The
/// runtime clones one `KevyCommands` per shard; the manual [`Clone`]
/// shares the state Arc and rebuilds the shard zone empty.
pub struct KevyCommands {
    state: Arc<RuntimeState>,
    shard: ShardCtx,
}

impl Clone for KevyCommands {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            // The shard-private zone is never copied: each clone is a
            // fresh shard identity, filled in by `on_shard_start`.
            shard: ShardCtx::default(),
        }
    }
}

impl Default for KevyCommands {
    fn default() -> Self {
        Self::new()
    }
}

impl KevyCommands {
    /// Single-shard command set over a default [`RuntimeState`] — the
    /// embedded / test entry (one keyspace, no I/O, no sidecars).
    pub fn new() -> Self {
        Self::sharded(1)
    }

    /// Default-state command set sized for an `nshards`-shard
    /// [`kevy_rt::Runtime`]. The default config's `server.threads` is
    /// set to `nshards` so shard-count-aware handlers (CLUSTER, the
    /// Lua cross-shard check) agree with the runtime.
    pub fn sharded(nshards: usize) -> Self {
        let mut state = RuntimeState::embedded(nshards);
        if nshards > 1 {
            let mut cfg = Config::default();
            cfg.server.threads = nshards;
            *state.config.get_mut().expect("config poisoned") = Arc::new(cfg);
        }
        Self::with_state(Arc::new(state))
    }

    /// Command set over an explicit shared state — the `serve` path,
    /// and the embedder form for a real config:
    /// `KevyCommands::with_state(Arc::new(RuntimeState::new(cfg, dir, n)?))`.
    pub fn with_state(state: Arc<RuntimeState>) -> Self {
        Self {
            state,
            shard: ShardCtx::default(),
        }
    }

    /// The shared state this command set serves.
    pub fn state(&self) -> &Arc<RuntimeState> {
        &self.state
    }

    /// Execute one command against `store`, returning the RESP reply.
    /// The direct (runtime-less) dispatch entry for embedding and
    /// tests.
    pub fn dispatch<A: kevy_resp::ArgvView + ?Sized>(
        &self,
        store: &mut kevy_store::Store,
        args: &A,
    ) -> Vec<u8> {
        crate::dispatch::dispatch(&self.ctx(), store, args)
    }

    /// [`Self::dispatch`] appending the reply to `out` (no per-command
    /// reply allocation).
    pub fn dispatch_into<A: kevy_resp::ArgvView + ?Sized>(
        &self,
        store: &mut kevy_store::Store,
        args: &A,
        out: &mut Vec<u8>,
    ) {
        crate::dispatch::dispatch_into(&self.ctx(), store, args, out);
    }

    pub(crate) fn shard_ctx(&self) -> &ShardCtx {
        &self.shard
    }

    pub(crate) fn ctx(&self) -> Ctx<'_> {
        Ctx {
            state: &self.state,
            shard: &self.shard,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_shares_state_but_not_shard_zone() {
        let c = KevyCommands::new();
        c.shard.set_shard_id(3);
        let c2 = c.clone();
        assert!(Arc::ptr_eq(&c.state, &c2.state));
        assert_eq!(c.shard.shard_id(), 3);
        assert_eq!(c2.shard.shard_id(), 0, "clone must start with a fresh shard zone");
    }

    #[test]
    fn embedded_state_has_no_sidecar_dir_and_no_explicit_config() {
        let c = KevyCommands::new();
        assert!(c.state.sidecar_dir().is_none());
        assert!(!c.state.config_is_explicit());
    }

    #[test]
    fn explicit_state_reports_config_and_sidecar_dir() {
        let cfg = Arc::new(Config::default());
        let state =
            RuntimeState::new(cfg, PathBuf::from("/tmp/kevy-x"), 2).expect("default scopes");
        assert!(state.config_is_explicit());
        assert_eq!(state.sidecar_dir(), Some(Path::new("/tmp/kevy-x")));
        assert_eq!(state.nshards(), 2);
    }

    #[test]
    fn config_replace_flips_explicit_and_swaps_value() {
        let c = KevyCommands::new();
        let mut cfg = Config::default();
        cfg.memory.maxmemory = 12345;
        c.state.config_replace(Arc::new(cfg));
        assert!(c.state.config_is_explicit());
        assert_eq!(c.state.config().memory.maxmemory, 12345);
    }

    #[test]
    fn ingest_guard_sets_and_clears_prefix() {
        let shard = ShardCtx::default();
        {
            let _g = shard.ingest_guard(b"app:".to_vec());
            assert!(shard.ingesting_matches(b"app:k1"));
            assert!(!shard.ingesting_matches(b"other:k"));
        }
        assert!(!shard.ingesting_matches(b"app:k1"));
    }

    #[test]
    fn sharded_state_mirrors_nshards_into_config_threads() {
        let c = KevyCommands::sharded(4);
        assert_eq!(c.state.config().server.threads, 4);
        assert!(!c.state.config_is_explicit());
    }
}
