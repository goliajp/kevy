//! [`Commands`] trait impl for [`KevyCommands`]. Split out of `lib.rs`
//! so that file stays under the 500-LOC project ceiling — `lib.rs` is
//! the crate entry (re-exports + `serve`/`drain_commands`/`handle_conn`
//! helpers); the trait impl that wires kevy's verbs into the runtime
//! lives here.

use kevy_rt::{
    ArgvView, BlockKind, Commands, ExtensionReduced, NotifyClass, ResolvedCmd, RespVersion, Route,
    TxnKind,
};
use kevy_store::Store;

use crate::cmd::{self, upper_verb};
use crate::{
    Argv, KevyCommands, cmd_block, cmd_block_serve, cmd_hello, cmd_resolve, dispatch,
    map_appendfsync, map_eviction_policy, ops,
};

impl Commands for KevyCommands {
    fn route<A: ArgvView + ?Sized>(&self, args: &A) -> Route {
        // One routing table for the whole crate: `cmd_resolve` owns it
        // (the hot path reads it through `resolve()`); this standalone
        // accessor delegates so the two faces can never drift.
        cmd_resolve::route(&self.state().replication, args)
    }

    fn dispatch<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8> {
        dispatch::dispatch(&self.ctx(), store, args)
    }

    fn dispatch_into<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A, out: &mut Vec<u8>) {
        dispatch::dispatch_into(&self.ctx(), store, args, out);
    }

    fn dispatch_into_resp3<A: ArgvView + ?Sized>(
        &self,
        store: &mut Store,
        args: &A,
        out: &mut Vec<u8>,
    ) {
        dispatch::dispatch_into_resp3(&self.ctx(), store, args, out);
    }

    fn is_quit<A: ArgvView + ?Sized>(&self, args: &A) -> bool {
        args.first()
            .is_some_and(|c| c.eq_ignore_ascii_case(b"QUIT"))
    }

    fn on_shard_init(&self, store: &mut Store) {
        // Snapshot the shared config and apply its `[memory]` section to
        // this shard. Outside `serve` (tests / embedded) the state holds
        // `Config::default()` (maxmemory=0), so the call is harmlessly a
        // no-op there.
        let cfg = self.state().config();
        store.set_max_memory(
            cfg.memory.maxmemory,
            map_eviction_policy(cfg.memory.maxmemory_policy),
        );
    }

    fn on_shard_start(&self, shard: usize) {
        // Thread-per-core: the reactor thread *is* the shard. The shard id
        // and this shard's INFO-stats slot (lock-free publish + counter
        // bumps, see `ops::stats`) land in this clone's ShardCtx.
        self.shard_ctx().set_shard_id(shard);
        self.shard_ctx().set_stats_slot(self.state().obs.slot(shard));
    }

    fn on_persist_stats(&self, in_flight: bool, aof_rewrites_total: u64) {
        // `INFO persistence` answers with the answering shard's view
        // (the COUNTKEYSINSLOT precedent), refreshed by the reactor tick.
        self.shard_ctx().set_persist_stats(in_flight, aof_rewrites_total);
    }

    fn on_replay_report(&self, dropped_bytes: u64, corrupt: bool) {
        // Boot-replay verdict for `INFO persistence` — non-zero drops are
        // the operator's alert signal (the store holds less than the AOF
        // did; the dropped region was quarantined on disk).
        self.shard_ctx().set_replay_report(dropped_bytes, corrupt);
    }

    fn on_replication_view(&self, master_repl_offset: u64, replicas: Vec<kevy_rt::ReplicaViewRow>) {
        // Publish to the shared per-shard slot FIRST — `ROLE` / `INFO
        // replication` answer on one shard but fold every shard's slot
        // into an instance-wide view (offset sum + per-replica union).
        self.state().obs.publish_repl_view(
            self.shard_ctx().shard_id(),
            crate::state::ReplShardView {
                offset: master_repl_offset,
                replicas: replicas.clone(),
            },
        );
        // The shard zone keeps its own copy for per-shard consumers
        // (the min-replicas health gate reads the answering shard's
        // rows — write gating is per shard-stream by design).
        self.shard_ctx()
            .set_replication_view(ops::replication::ReplicationView { replicas });
        // v3-cluster Phase 1.5: feed the offset into kevy-elect so
        // the next heartbeat carries the up-to-date `repl_offset`.
        // No-op when the elector isn't running.
        self.state().election.set_view_offset(
            &self.state().replication,
            self.shard_ctx().shard_id(),
            master_repl_offset,
        );
    }

    fn on_command(&self) {
        self.shard_ctx().add_command();
    }

    fn on_connection(&self) {
        self.shard_ctx().add_connection();
    }

    fn shard_tick_interval_ms(&self) -> u64 {
        // hz=0 disables the active reaper (lazy expiry still runs); else
        // every `1000/hz` ms — capped at 10 s so a misconfig can't park the
        // reactor's tick check loop forever.
        let cfg = self.state().config();
        let hz = cfg.expiry.hz;
        if hz == 0 {
            0
        } else {
            (1000 / u64::from(hz)).clamp(1, 10_000)
        }
    }

    fn on_write(&self, store: &mut Store, key: &[u8]) {
        // Zero-tax gate: with no index/view declared, a write costs
        // one cached-bit branch here instead of the two global flag
        // loads the runtimes used to pay.
        let bits = self.gate_bits();
        if bits & crate::state::IDX_NONEMPTY != 0 {
            crate::index_runtime::on_write(&self.ctx(), store, key);
        }
        if bits & crate::state::VIEW_NONEMPTY != 0 {
            // Views probe the segments the line above just refreshed.
            crate::view_runtime::on_write(&self.ctx(), store, key);
        }
    }

    fn geo_search(&self, store: &mut Store, argv: &[Vec<u8>]) -> kevy_rt::GeoHits {
        crate::dispatch_geo::geo_search(store, argv)
    }

    fn extension_op(&self, store: &mut Store, argv: &[Vec<u8>]) -> Vec<u8> {
        if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"PREFIX.DIGEST")) {
            return crate::cmd_digest::extension_op(store, argv);
        }
        if argv.first().is_some_and(|v| v.len() > 5 && v[..5].eq_ignore_ascii_case(b"VIEW.")) {
            return crate::cmd_view::extension_op(&self.ctx(), store, argv);
        }
        if argv.first().is_some_and(|v| v.len() > 6 && v[..6].eq_ignore_ascii_case(b"TABLE.")) {
            return crate::cmd_table::extension_op(&self.ctx(), store, argv);
        }
        crate::cmd_index_query::extension_op(&self.ctx(), store, argv)
    }

    fn extension_reduce(
        &self,
        argv: &[Vec<u8>],
        chunks: Vec<Vec<u8>>,
        proto: kevy_resp::RespVersion,
    ) -> ExtensionReduced {
        let catalogs = &self.state().catalogs;
        let reduced = if argv.first().is_some_and(|v| v.eq_ignore_ascii_case(b"PREFIX.DIGEST")) {
            ExtensionReduced::Reply(crate::cmd_digest::extension_reduce(chunks))
        } else if argv.first().is_some_and(|v| v.len() > 5 && v[..5].eq_ignore_ascii_case(b"VIEW."))
        {
            crate::cmd_view::extension_reduce(catalogs, argv, chunks)
        } else if argv
            .first()
            .is_some_and(|v| v.len() > 6 && v[..6].eq_ignore_ascii_case(b"TABLE."))
        {
            crate::cmd_table::extension_reduce(catalogs, argv, chunks)
        } else {
            crate::cmd_index_reduce::extension_reduce(catalogs, argv, chunks)
        };
        match reduced {
            ExtensionReduced::Reply(reply) if proto == kevy_resp::RespVersion::V3 => {
                ExtensionReduced::Reply(crate::cmd_index_reduce::resp3_upgrade(argv, reply))
            }
            other => other,
        }
    }

    fn write_denied(&self) -> Option<Vec<u8>> {
        // Two-tier verdict: the cached bit answers "certainly
        // allowed" with a single epoch load; only a raised gate walks
        // the precise fence-ordering judge (which may lock the quiesce
        // slot / count replicas to render the exact error).
        if self.gate_bits() & crate::state::WRITE_GATED == 0 {
            return None;
        }
        self.state().replication.write_denied_reply(|| {
            let max_lag_ms = self.state().config().replication.min_replicas_max_lag_ms;
            self.shard_ctx().healthy_replica_count(max_lag_ms)
        })
    }

    fn read_denied<A: ArgvView + ?Sized>(&self, args: &A) -> Option<Vec<u8>> {
        // READ_GATED = bounded-staleness replica or a replica inside a
        // full-resync snapshot load. The staleness deadline is a time
        // condition and the loading flag flips mid-window, so the slow
        // path re-loads the live values on every gated read.
        if self.gate_bits() & crate::state::READ_GATED == 0 {
            return None;
        }
        // PING / INFO / HELLO stay answerable while gated — health
        // checks and monitoring must keep working during a snapshot
        // load (and a stale replica still proves liveness). CLIENT /
        // CONFIG / SHUTDOWN stay answerable too: an operator must be
        // able to inspect connections, kill a misbehaving one, or
        // stop the node while a load is in flight. Matches the verbs
        // Redis flags loading-exempt.
        if args.get(0).is_some_and(|v| {
            v.eq_ignore_ascii_case(b"PING")
                || v.eq_ignore_ascii_case(b"INFO")
                || v.eq_ignore_ascii_case(b"HELLO")
                || v.eq_ignore_ascii_case(b"CLIENT")
                || v.eq_ignore_ascii_case(b"CONFIG")
                || v.eq_ignore_ascii_case(b"SHUTDOWN")
        }) {
            return None;
        }
        self.state().replication.read_denied_reply()
    }

    fn on_shard_tick(&self, store: &mut Store) {
        let bits = self.gate_bits();
        if bits & crate::state::IDX_NONEMPTY != 0 {
            crate::index_runtime::on_tick(&self.ctx(), store);
        }
        if bits & crate::state::VIEW_NONEMPTY != 0 {
            crate::view_runtime::on_tick(&self.ctx(), store);
        }
        // Run Redis's `activeExpireCycle` per shard. `sample` controls the
        // batch size; up to 16 rounds per tick is well below Redis's 25 %
        // CPU budget at the default 10 Hz cadence. Cheap when no TTL'd
        // keys exist (a single map-emptiness check + bucket walk).
        let cfg = self.state().config();
        // A replica does NOT actively expire — the primary
        // owns TTL truth and ships DEL/expiry effects through the
        // replication feed (Redis semantics; diverging reapers would
        // fork the keyspaces). Lazy-expiry reads stay local either way.
        if !self.state().replication.is_replica() {
            let samples = cfg.expiry.sample as usize;
            store.tick_expire(samples, 16);
            // Sweep due hash field TTLs. Deadlines live in the AOF
            // (HPEXPIREAT frames), so replay purges identically — no
            // logging needed here, same determinism argument as key TTLs.
            let _ = store.tick_hash_ttl(64);
        }
        // Tiering (T5): budget re-resolution + the index/view floor
        // feed (body in `tier_tick` below — 50-LOC rule), then the
        // tick continuation of the budgeted spill (no-op when tiering
        // is off or under the watermark). Replicas tier too — applied
        // frames grow their keyspace like any write.
        tier_tick(self, store, bits, &cfg);
        store.demote_step();
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
        ops::stats::publish_gauges(self.shard_ctx(), store);
        // The lead shard advances the process-wide ops-per-sec sampler.
        ops::stats::sample_ops_if_lead(self.shard_ctx(), &self.state().obs);
    }

    fn shutdown_save_requested(&self) -> bool {
        self.state().shutdown_save_requested()
    }

    fn on_conn_gauge(&self, live: u64) {
        self.shard_ctx().with_stats_slot(|s| {
            s.clients_connected
                .store(live, std::sync::atomic::Ordering::Relaxed);
        });
    }

    fn live_runtime_config(&self) -> kevy_rt::LiveRuntimeConfig {
        // Per-tick (every 100 ms by default) re-read of the shared config.
        // When no explicit config was ever installed (tests, hand-rolled
        // `Runtime`s in examples), return all-None so the builder's
        // explicit `with_appendfsync` / `with_auto_aof_rewrite` choices
        // aren't silently clobbered by `Config::default()` values. Once an
        // explicit config exists, every field is wrapped in `Some` so the
        // shard re-applies CONFIG SET changes within one tick.
        if !self.state().config_is_explicit() {
            // The promotion counter still flows — it doesn't
            // clobber any builder choice, and an embedded promotion
            // must fence feed generations too.
            return kevy_rt::LiveRuntimeConfig {
                promotion_epoch: self.state().replication.promotion_epoch(),
                ..kevy_rt::LiveRuntimeConfig::default()
            };
        }
        let cfg = self.state().config();
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
            auto_aof_rewrite_bytes: Some(cfg.persistence.auto_aof_rewrite_bytes),
            auto_aof_rewrite_interval_secs: Some(cfg.persistence.auto_aof_rewrite_interval_secs),
            tick_interval_ms: tick_ms,
            // A flag string with an unknown char can't be installed —
            // config admission validates it — so the fallback default
            // (notifications OFF) is unreachable in practice and safe
            // if a foreign path ever slips one through.
            notify_flags: Some(
                kevy_config::parse_notification_flags(
                    &cfg.notification.notify_keyspace_events,
                )
                .unwrap_or_default(),
            ),
            slowlog_slower_than_micros: Some(cfg.slowlog.slower_than_micros),
            slowlog_max_len: Some(cfg.slowlog.max_len),
            promotion_epoch: self.state().replication.promotion_epoch(),
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

    /// Queue-time validation for `MULTI`: reject a command that can
    /// never queue so `EXEC` aborts with `-EXECABORT` (Redis
    /// `CLIENT_DIRTY_EXEC`). Two unambiguous cases:
    ///   * unknown verb — no [`crate::verb_meta`] row;
    ///   * too few args — below the verb's minimum arity (Redis
    ///     convention: positive `n` needs exactly `n` argv elements
    ///     verb-included, negative `n` needs at least `|n|`; either way
    ///     the minimum is `|n|`, and no handler accepts fewer).
    ///
    /// The "too many args for an exact-arity verb" case is deliberately
    /// NOT rejected here: `VERB_META.arity` is parity-checked for
    /// coverage, not verified exact against every handler's real argv
    /// acceptance, so an over-strict exact check could abort a
    /// variadic command that would in fact execute. Those still surface
    /// their own arity error at `EXEC` time — an error inside the
    /// transaction, not a false abort of it.
    fn queue_error<A: ArgvView + ?Sized>(&self, args: &A) -> Option<Vec<u8>> {
        let name = args.first()?;
        let mut buf = [0u8; 32];
        let upper = upper_verb(name, &mut buf);
        let shown = String::from_utf8_lossy(name);
        let known = std::str::from_utf8(upper)
            .ok()
            .and_then(crate::verb_meta::verb_meta);
        let Some(meta) = known else {
            return Some(format!("-ERR unknown command '{shown}'\r\n").into_bytes());
        };
        let min_args = i64::from(meta.arity.unsigned_abs());
        (( args.len() as i64) < min_args).then(|| {
            format!("-ERR wrong number of arguments for '{}' command\r\n", meta.name.to_lowercase())
                .into_bytes()
        })
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

    fn block_restore_argv(
        &self,
        store: &mut Store,
        kind: BlockKind,
        key: &[u8],
    ) -> Option<Argv> {
        cmd_block_serve::block_restore_argv(store, kind, key)
    }

    fn block_ready<A: ArgvView + ?Sized>(
        &self,
        store: &mut Store,
        serve_argv: &A,
        kind: BlockKind,
    ) -> bool {
        cmd_block_serve::block_ready(&self.ctx(), store, serve_argv, kind)
    }

    /// One-pass verb resolution — the reactor calls this once per cmd and
    /// reads back txn_kind / route / is_quit / is_write without re-scanning
    /// the verb. This is `kevy-rt`'s primary hot-path optimization: every
    /// match arm uses the same `upper` buffer. Body in `cmd_resolve`.
    fn resolve<A: ArgvView + ?Sized>(&self, args: &A) -> ResolvedCmd {
        cmd_resolve::kevy_resolve(&self.state().replication, args)
    }
}

/// The shard tick's tiering upkeep (T5, RFC §1 D3): re-resolve the
/// budget spec — auto/percent re-probe the cgroup/meminfo bound so
/// live limit changes are honored (the maxmemory reapply precedent) —
/// and feed the index/view memory floor into the unified watermark.
/// Gated on tiering being on: an untiered tick pays one branch.
fn tier_tick(c: &KevyCommands, store: &mut Store, bits: u32, cfg: &kevy_config::Config) {
    if !store.tier_enabled() {
        return;
    }
    if let Ok(Some(total)) = crate::resolve_tier_budget(cfg) {
        let n = c.state().nshards().max(1) as u64;
        store.set_tier_budget((total / n).max(1));
    }
    let mut reserved = 0u64;
    if bits & crate::state::IDX_NONEMPTY != 0 {
        reserved += crate::index_runtime::reserved_bytes(&c.ctx(), store);
    }
    if bits & crate::state::VIEW_NONEMPTY != 0 {
        reserved += crate::view_runtime::reserved_bytes(&c.ctx());
    }
    store.set_tier_reserved(reserved);
}
