//! [`Commands`] trait impl for [`KevyCommands`]. Split out of `lib.rs`
//! so that file stays under the 500-LOC project ceiling — `lib.rs` is
//! the crate entry (re-exports + `serve`/`drain_commands`/`handle_conn`
//! helpers); the trait impl that wires kevy's verbs into the runtime
//! lives here.

use kevy_rt::{
    ArgvView, BlockKind, Commands, NotifyClass, ResolvedCmd, RespVersion, Route, TxnKind,
    parse_slowlog_sub,
};
use kevy_store::Store;

use crate::cmd::{self, scan_pattern, upper_verb};
use crate::{
    Argv, KevyCommands, cmd_block, cmd_block_serve, cmd_hello, cmd_resolve, config_global,
    dispatch, map_appendfsync, map_eviction_policy, ops,
};

impl Commands for KevyCommands {
    fn route<A: ArgvView + ?Sized>(&self, args: &A) -> Route {
        let Some(name) = args.first() else {
            return Route::Local;
        };
        let mut buf = [0u8; 32];
        match upper_verb(name, &mut buf) {
            b"HELLO" => Route::Hello,
            b"PING" | b"ECHO" | b"QUIT" | b"COMMAND" | b"CONFIG"
            | b"INFO" | b"CLUSTER" | b"DEBUG" | b"WAIT" | b"SHUTDOWN"
            | b"CLIENT" | b"SELECT" | b"ROLE"
            | b"REPLICAOF" | b"SLAVEOF" => Route::Local,
            b"DBSIZE" => Route::Dbsize,
            b"FLUSHDB" | b"FLUSHALL" => Route::Flush,
            b"SAVE" => Route::Save,
            b"BGSAVE" => Route::BgSave,
            b"BGREWRITEAOF" => Route::RewriteAof,
            // Cross-shard multi-key (malformed arity falls back to Local so the
            // dispatch stub returns the arity error).
            b"MSET" if args.len() >= 3 && !args.len().is_multiple_of(2) => Route::MSet,
            b"MGET" if args.len() >= 2 => Route::MGet,
            b"SINTER" if args.len() >= 2 => Route::SInter,
            b"SUNION" if args.len() >= 2 => Route::SUnion,
            b"SDIFF" if args.len() >= 2 => Route::SDiff,
            b"KEYS" if args.len() == 2 => Route::Keys(Some(args[1].to_vec())),
            b"SCAN" if args.len() >= 2 => Route::Scan(scan_pattern(args)),
            b"RANDOMKEY" if args.len() == 1 => Route::RandomKey,
            b"SUBSCRIBE" if args.len() >= 2 => Route::Subscribe,
            b"UNSUBSCRIBE" => Route::Unsubscribe, // no args = unsubscribe all
            b"PSUBSCRIBE" if args.len() >= 2 => Route::Psubscribe,
            b"PUNSUBSCRIBE" => Route::Punsubscribe, // no args = punsubscribe all
            b"PUBLISH" if args.len() == 3 => Route::Publish,
            b"WATCH" if args.len() >= 2 => Route::Watch,
            b"UNWATCH" => Route::Unwatch,
            b"RENAME" => Route::Rename { nx: false },
            b"RENAMENX" => Route::Rename { nx: true },
            // v1.27.1: keep in sync with cmd_resolve::route_for_verb —
            // the runtime hot path goes through `resolve()` →
            // `route_for_verb`, but tests + future direct callers of
            // `route()` need the same answer.
            b"EVAL" | b"EVALSHA" | b"EVAL_RO" | b"EVALSHA_RO" => {
                if args.len() >= 4 {
                    let nk = std::str::from_utf8(&args[2])
                        .ok()
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(0);
                    if nk >= 1 && (args.len() as i64) >= 3 + nk {
                        Route::Single(3)
                    } else {
                        Route::Local
                    }
                } else {
                    Route::Local
                }
            }
            b"SCRIPT" => Route::Local,
            b"XREAD" => cmd_block::xread_route(args),
            b"XREADGROUP" => cmd_block::xreadgroup_route(args),
            // XGROUP / XINFO key is at args[2] (after the subcommand).
            b"XGROUP" | b"XINFO" if args.len() >= 3 => Route::Single(2),
            b"SLOWLOG" => Route::Slowlog(parse_slowlog_sub(args)),
            // DEL/EXISTS are single-key (fast path) unless given multiple keys.
            b"DEL" | b"UNLINK" => {
                if args.len() == 2 {
                    Route::Single(1)
                } else {
                    Route::DelKeys
                }
            }
            b"EXISTS" => {
                if args.len() == 2 {
                    Route::Single(1)
                } else {
                    Route::ExistsKeys
                }
            }
            // All remaining commands act on a single key at args[1].
            _ => {
                if args.len() >= 2 {
                    Route::Single(1)
                } else {
                    Route::Local // malformed; dispatch will return the error
                }
            }
        }
    }

    fn dispatch<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8> {
        dispatch::dispatch(store, args)
    }

    fn dispatch_into<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A, out: &mut Vec<u8>) {
        dispatch::dispatch_into(store, args, out);
    }

    fn dispatch_resp3<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        dispatch::dispatch_into_resp3(store, args, &mut out);
        out
    }

    fn dispatch_into_resp3<A: ArgvView + ?Sized>(
        &self,
        store: &mut Store,
        args: &A,
        out: &mut Vec<u8>,
    ) {
        dispatch::dispatch_into_resp3(store, args, out);
    }

    fn is_quit<A: ArgvView + ?Sized>(&self, args: &A) -> bool {
        args.first()
            .is_some_and(|c| c.eq_ignore_ascii_case(b"QUIT"))
    }

    fn on_shard_init(&self, store: &mut Store) {
        // Snapshot the process-wide config and apply its `[memory]` section
        // to this shard. Reading `config_global::get()` returns
        // `Config::default()` (maxmemory=0) when running outside `serve` —
        // e.g. tests / embedded — so the call is harmlessly a no-op there.
        let cfg = config_global::get();
        store.set_max_memory(
            cfg.memory.maxmemory,
            map_eviction_policy(cfg.memory.maxmemory_policy),
        );
    }

    fn on_shard_start(&self, shard: usize) {
        // Thread-per-core: the reactor thread *is* the shard, so a
        // thread-local carries per-shard identity into dispatch handlers
        // (CLUSTER MYID / the `myself` flag in CLUSTER NODES).
        ops::cluster::set_current_shard(shard);
        // Cache this shard's INFO-stats slot for lock-free publish + counter
        // bumps (see `ops::stats`).
        ops::stats::register_shard(shard);
    }

    fn on_persist_stats(&self, in_flight: bool, aof_rewrites_total: u64) {
        // Same thread-local pattern as `on_shard_start`: `INFO persistence`
        // answers with the answering shard's view (the COUNTKEYSINSLOT
        // precedent), refreshed by the reactor tick.
        ops::set_persist_stats(in_flight, aof_rewrites_total);
    }

    fn on_replication_view(
        &self,
        master_repl_offset: u64,
        replicas: Vec<(std::net::Ipv4Addr, u16, u64)>,
    ) {
        // Same thread-local pattern as `on_persist_stats`: `ROLE` /
        // `INFO replication` read the answering shard's most-recent
        // view. T1.28.5 added the per-replica list — `connected_slaves`
        // is derived from `replicas.len()` at read time.
        ops::replication::set_replication_view(master_repl_offset, replicas);
        // v3-cluster Phase 1.5: feed the offset into kevy-elect so
        // the next heartbeat carries the up-to-date `repl_offset`.
        // No-op when the elector isn't running.
        crate::elect_integration::set_view_offset(master_repl_offset);
    }

    fn on_command(&self) {
        ops::stats::add_command();
    }

    fn on_connection(&self) {
        ops::stats::add_connection();
    }

    fn shard_tick_interval_ms(&self) -> u64 {
        // hz=0 disables the active reaper (lazy expiry still runs); else
        // every `1000/hz` ms — capped at 10 s so a misconfig can't park the
        // reactor's tick check loop forever.
        let cfg = config_global::get();
        let hz = cfg.expiry.hz;
        if hz == 0 {
            0
        } else {
            (1000 / u64::from(hz)).clamp(1, 10_000)
        }
    }

    fn on_shard_tick(&self, store: &mut Store) {
        // Run Redis's `activeExpireCycle` per shard. `sample` controls the
        // batch size; up to 16 rounds per tick is well below Redis's 25 %
        // CPU budget at the default 10 Hz cadence. Cheap when no TTL'd
        // keys exist (a single map-emptiness check + bucket walk).
        let cfg = config_global::get();
        let samples = cfg.expiry.sample as usize;
        store.tick_expire(samples, 16);
        // Re-apply maxmemory + eviction policy in case `CONFIG SET` has
        // swapped the global since the previous tick. `store.set_max_memory`
        // is idempotent and cheap (compares + assigns two scalars + may
        // recompute soft-limit accounting); paying it every 100 ms is well
        // below the noise floor of any benchmark.
        store.set_max_memory(
            cfg.memory.maxmemory,
            map_eviction_policy(cfg.memory.maxmemory_policy),
        );
        // Publish this shard's gauges (used_memory, key/expire counts, …) so
        // `INFO`, answered on any one shard, can sum the process-wide view.
        ops::stats::publish_gauges(store);
        // The lead shard advances the process-wide ops-per-sec sampler.
        ops::stats::sample_ops_if_lead();
    }

    fn live_runtime_config(&self) -> kevy_rt::LiveRuntimeConfig {
        // Per-tick (every 100 ms by default) re-read of the process-wide
        // config. When the embedder hasn't called `config_init` (tests,
        // hand-rolled `Runtime`s in examples), return all-None so the
        // builder's explicit `with_appendfsync` / `with_auto_aof_rewrite`
        // choices aren't silently clobbered by `Config::default()` values.
        // Once `config_init` has run, every field is wrapped in `Some` so
        // the shard re-applies CONFIG SET changes within one tick.
        if !config_global::is_initialised() {
            return kevy_rt::LiveRuntimeConfig::default();
        }
        let cfg = config_global::get();
        let hz = cfg.expiry.hz;
        let tick_ms = if hz == 0 {
            Some(0)
        } else {
            Some((1000u64 / u64::from(hz)).clamp(1, 10_000))
        };
        kevy_rt::LiveRuntimeConfig {
            appendfsync: Some(map_appendfsync(cfg.persistence.appendfsync)),
            auto_aof_rewrite_pct: Some(cfg.persistence.auto_aof_rewrite_percentage),
            auto_aof_rewrite_min_size: Some(cfg.persistence.auto_aof_rewrite_min_size),
            tick_interval_ms: tick_ms,
            notify_flags: Some(kevy_config::parse_notification_flags(
                &cfg.notification.notify_keyspace_events,
            )),
            slowlog_slower_than_micros: Some(cfg.slowlog.slower_than_micros),
            slowlog_max_len: Some(cfg.slowlog.max_len),
        }
    }

    fn hello_reply<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        current_proto: RespVersion,
    ) -> (RespVersion, Vec<u8>) {
        cmd_hello::kevy_hello_reply(args, current_proto)
    }

    fn is_write<A: ArgvView + ?Sized>(&self, args: &A) -> bool {
        let Some(name) = args.first() else {
            return false;
        };
        let mut buf = [0u8; 32];
        cmd::is_write_verb(upper_verb(name, &mut buf))
    }

    fn notify_class<A: ArgvView + ?Sized>(&self, args: &A) -> Option<NotifyClass> {
        let name = args.first()?;
        let mut buf = [0u8; 32];
        cmd::notify_class_for_verb(upper_verb(name, &mut buf))
    }

    fn txn_kind<A: ArgvView + ?Sized>(&self, args: &A) -> TxnKind {
        let Some(name) = args.first() else {
            return TxnKind::Other;
        };
        let mut buf = [0u8; 32];
        match upper_verb(name, &mut buf) {
            b"MULTI" => TxnKind::Multi,
            b"EXEC" => TxnKind::Exec,
            b"DISCARD" => TxnKind::Discard,
            b"WATCH" => TxnKind::Watch,
            _ => TxnKind::Other,
        }
    }

    /// Freeze `$` IDs in an `XREAD BLOCK` argv at park time. Default
    /// would leave `$` literal in the parked argv; the wake retry would
    /// then re-resolve `$` to the *post-XADD* `last_id`, miss the new
    /// entry, and time out instead of returning it. Other block kinds
    /// (BLPOP / BRPOP / XREADGROUP `>`) have no state-dependent argv —
    /// they fall through to the trait default.
    fn resolve_block_argv<A: ArgvView + ?Sized>(
        &self,
        store: &mut Store,
        args: &A,
        kind: BlockKind,
    ) -> Argv {
        match kind {
            BlockKind::XReadBlock => cmd_block::xread_resolve_argv(store, args),
            _ => args.to_argv(),
        }
    }

    fn block_serve_argv<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        kind: BlockKind,
        key: &[u8],
    ) -> Argv {
        cmd_block_serve::block_serve_argv(args, kind, key)
    }

    fn block_ready<A: ArgvView + ?Sized>(
        &self,
        store: &mut Store,
        serve_argv: &A,
        kind: BlockKind,
    ) -> bool {
        cmd_block_serve::block_ready(store, serve_argv, kind)
    }

    fn wake_idx<A: ArgvView + ?Sized>(&self, args: &A) -> Option<u8> {
        let name = args.first()?;
        let mut buf = [0u8; 32];
        cmd_block::wake_idx_for_verb(upper_verb(name, &mut buf))
    }

    /// One-pass verb resolution — the reactor calls this once per cmd and
    /// reads back txn_kind / route / is_quit / is_write without re-scanning
    /// the verb. This is `kevy-rt`'s primary hot-path optimization: every
    /// match arm uses the same `upper` buffer. Body in `cmd_resolve`.
    fn resolve<A: ArgvView + ?Sized>(&self, args: &A) -> ResolvedCmd {
        cmd_resolve::kevy_resolve(args)
    }
}
