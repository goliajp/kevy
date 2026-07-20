//! The [`Commands`] trait — the seam between the runtime and a command
//! implementation. Split from `lib.rs` for the 500-LOC house rule.

use crate::{
    BlockHint, BlockKind, ExtensionReduced, GeoHits, LiveRuntimeConfig, NotifyClass,
    ReplicaViewRow, ResolvedCmd, Route, Store, TxnKind,
};
use kevy_resp::{Argv, ArgvView, RespVersion};

/// Command-set semantics injected into the runtime. Cloned to every core, so it
/// must be cheap/stateless to clone.
pub trait Commands: Clone + Send + 'static {
    /// Classify how a command is routed across shards.
    fn route<A: ArgvView + ?Sized>(&self, args: &A) -> Route;
    /// Execute a full command against one shard's store, returning RESP bytes.
    fn dispatch<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8>;
    /// Execute a command, appending the RESP reply to `out`. The in-order local
    /// fast path uses this to write straight into the connection's output buffer
    /// (no per-command reply `Vec`). Default: delegate to [`dispatch`](Self::dispatch).
    fn dispatch_into<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.dispatch(store, args));
    }
    /// RESP3 variant of [`Self::dispatch_into`] — called when the
    /// connection has negotiated `HELLO 3`. Default: delegate to the
    /// RESP2 path (so a server that hasn't migrated any replies still
    /// works correctly with a RESP3 client, per spec). Override per
    /// command to emit RESP3 shapes (Map / Set / Double / …).
    fn dispatch_into_resp3<A: ArgvView + ?Sized>(
        &self,
        store: &mut Store,
        args: &A,
        out: &mut Vec<u8>,
    ) {
        self.dispatch_into(store, args, out);
    }
    /// Classify a command for keyspace notifications. Returns `Some`
    /// for write commands that should fire a notification when the
    /// corresponding flag is enabled; `None` for read-only / no-op /
    /// not-yet-classified commands (those never publish). Default
    /// `None` so non-kevy embedders pay nothing.
    fn notify_class<A: ArgvView + ?Sized>(&self, _args: &A) -> Option<NotifyClass> {
        None
    }

    /// Handle `HELLO` — return the new connection protocol version + the
    /// reply bytes. The runtime applies the new version to the conn
    /// before scheduling the reply, so a `HELLO 3` ack itself comes out
    /// shaped as a RESP3 Map (the new protocol is in effect for its own
    /// reply).
    ///
    /// Default: ignore the args, keep `current_proto`, emit a minimal
    /// RESP2 +OK so embedders that don't care still see a sane reply.
    /// kevy's own impl in `kevy::KevyCommands` parses the optional
    /// protover and emits the full server-info shape.
    fn hello_reply<A: ArgvView + ?Sized>(
        &self,
        _args: &A,
        current_proto: RespVersion,
    ) -> (RespVersion, Vec<u8>) {
        (current_proto, b"+OK\r\n".to_vec())
    }
    /// Whether this command should close the connection (QUIT).
    fn is_quit<A: ArgvView + ?Sized>(&self, args: &A) -> bool;
    /// Whether this command mutates the keyspace (so it must be logged to the AOF).
    fn is_write<A: ArgvView + ?Sized>(&self, args: &A) -> bool;
    /// Transaction-control classification (MULTI/EXEC/DISCARD vs anything else).
    fn txn_kind<A: ArgvView + ?Sized>(&self, args: &A) -> TxnKind;
    /// Called once per shard, immediately after [`Store::new`], before the
    /// reactor enters its event loop. Implementations install per-shard
    /// configuration that the runtime doesn't know about — currently the
    /// `maxmemory` + eviction-policy pair, which kevy ships via its own
    /// process-wide config snapshot. Default: no-op so non-kevy embedders
    /// aren't forced to override.
    fn on_shard_init(&self, _store: &mut Store) {}

    /// Called once on the shard's own thread, first thing in the reactor
    /// entry (both reactors), before restore/replay. Implementations that
    /// need per-shard identity at dispatch time (e.g. kevy's `CLUSTER MYID`
    /// / `CLUSTER NODES` `myself` flag) stash `shard` in a thread-local here
    /// — in a thread-per-core runtime the current thread *is* the shard.
    /// Default: no-op.
    fn on_shard_start(&self, _shard: usize) {}

    /// Per-tick persistence-stats publication: whether this shard has a
    /// background save/rewrite in flight and how many AOF rewrites have
    /// completed since open. Command layers that serve `INFO persistence`
    /// stash these in a thread-local (thread-per-core: the answering
    /// thread *is* the shard, same pattern as [`Self::on_shard_start`]).
    /// Default: no-op.
    fn on_persist_stats(&self, _in_flight: bool, _aof_rewrites_total: u64) {}

    /// One-shot boot-replay verdict for this shard: bytes dropped past
    /// the last replayable AOF frame (quarantined + truncated by the
    /// repair) and whether the stop was a corrupt frame. Fires once,
    /// after the shard's startup replay, before the listener accepts.
    /// Non-zero drops mean the shard recovered less than its file held —
    /// command layers surface it via `INFO persistence` so operators can
    /// alert on it. Default: no-op.
    fn on_replay_report(&self, _dropped_bytes: u64, _corrupt: bool) {}

    /// Per-tick live-connection gauge: how many client conns this
    /// shard currently holds (cluster-bus links excluded). Command
    /// layers publish it to their cross-shard stats slots so `INFO`
    /// `connected_clients` sums a real instance-wide value. Default:
    /// no-op.
    fn on_conn_gauge(&self, _live: u64) {}

    /// Per-tick replication-view publication: the answering shard's
    /// current `master_repl_offset` (== `ReplicationSource::next_offset()`)
    /// plus a [`ReplicaViewRow`] for every handshake-complete replica
    /// conn (in `AckSent`, `Streaming`, or `SnapshotShipping`); the
    /// row's `ack` is `None` until the replica's first `REPLCONF ACK`.
    /// Only called when this shard has a `ReplicationSource`
    /// installed (i.e. `Runtime::with_replication(true, ...)` was
    /// requested); standalone setups pay nothing. Command layers
    /// that serve `ROLE` / `INFO replication` stash the values in a
    /// thread-local (thread-per-core: the answering thread *is* the
    /// shard, same pattern as [`Self::on_persist_stats`]) and may
    /// additionally publish them to a shared slot for cross-shard
    /// aggregation. Default no-op.
    fn on_replication_view(&self, _master_repl_offset: u64, _replicas: Vec<ReplicaViewRow>) {}

    /// Periodic shard housekeeping (the equivalent of Redis's `serverCron`).
    /// kevy uses this to run [`Store::tick_expire`] at the configured
    /// `[expiry].hz`. Default no-op so non-kevy embedders / runtimes can
    /// ignore it.
    fn on_shard_tick(&self, _store: &mut Store) {}

    /// Polled once per shard as it leaves the reactor loop: `true` when
    /// the operator requested a final snapshot before exit (`SHUTDOWN
    /// SAVE`). The shard then runs one background save and drains it
    /// before the process exits. Default `false` — plain stops (SIGTERM,
    /// bare SHUTDOWN) drain in-flight persistence but don't force a new
    /// snapshot.
    fn shutdown_save_requested(&self) -> bool {
        false
    }

    /// Per-shard half of an extension fan-out command (IDX.* /
    /// future VIEW.* / FT.*): compute this shard's raw chunk for
    /// `argv`. The payload encoding is the embedder's own — the
    /// runtime treats it as opaque bytes and hands all chunks to
    /// [`Commands::extension_reduce`] at the origin.
    fn extension_op(&self, _store: &mut Store, _argv: &[Vec<u8>]) -> Vec<u8> {
        Vec::new()
    }

    /// Search half of a geo `*STORE` (`GEOSEARCHSTORE` / `GEORADIUS…STORE`),
    /// run on the SOURCE key's shard: match `argv`'s query against the source
    /// zset and return the `(member, score)` pairs to write — the scores
    /// already in their final form (geohash, or the STOREDIST distance in the
    /// unit the command asked for). The runtime writes them at the
    /// destination's own shard; see [`crate::exec_geostore`]. A command set
    /// that doesn't route [`Route::GeoStore`] never sees this call.
    fn geo_search(&self, _store: &mut Store, _argv: &[Vec<u8>]) -> GeoHits {
        GeoHits::Error(b"-ERR unknown command\r\n".to_vec())
    }

    /// Pre-dispatch write gate. `Some(err_bytes)` rejects every
    /// data-write client command with that RESP error before any
    /// routing (replication apply does NOT pass through here, so a
    /// read-only replica keeps applying its feed). Admin verbs
    /// (REPLICAOF / CONFIG) are not classified as writes and stay
    /// available as the operator escape hatch. Default: writes always
    /// allowed.
    fn write_denied(&self) -> Option<Vec<u8>> {
        None
    }

    /// Read-availability gate: called before READ verbs; return
    /// `Some(error_bytes)` to refuse the read (a replica whose feed is
    /// staler than the configured bound answers `-STALE`; one mid-way
    /// through a full-resync snapshot load answers `-LOADING`).
    /// `args` lets implementations exempt health-check verbs (PING)
    /// from the refusal. Default: reads always allowed.
    fn read_denied<A: ArgvView + ?Sized>(&self, _args: &A) -> Option<Vec<u8>> {
        None
    }

    /// Origin-side reduce of an extension fan-out — merge every
    /// shard's chunk (produced by [`Self::extension_op`]) into either
    /// the final RESP reply or a follow-up fan-out argv (see
    /// [`ExtensionReduced`]). `proto` is the requesting connection's
    /// negotiated RESP version so proto-aware reduces can shape the
    /// reply (Map vs pair-array).
    fn extension_reduce(
        &self,
        _argv: &[Vec<u8>],
        _chunks: Vec<Vec<u8>>,
        _proto: kevy_resp::RespVersion,
    ) -> ExtensionReduced {
        ExtensionReduced::Reply(b"-ERR extension commands not supported\r\n".to_vec())
    }

    /// Called after every applied write with the written key
    /// (when the resolver knew one). Default no-op; kevy uses it for
    /// synchronous secondary-index maintenance (derived-by-
    /// construction). Runs on the shard thread with store access —
    /// implementations must be cheap when their feature is off.
    fn on_write(&self, _store: &mut Store, _key: &[u8]) {}

    /// Called once per client command at dispatch entry (before routing /
    /// fan-out, so a multi-key command counts once). kevy uses it for
    /// `INFO stats: total_commands_processed`. Hot path — keep it to a single
    /// thread-local bump. Default no-op so non-kevy embedders pay nothing.
    fn on_command(&self) {}

    /// Called once per accepted client connection. kevy uses it for
    /// `INFO stats: total_connections_received`. Default no-op.
    fn on_connection(&self) {}

    /// Interval between [`Self::on_shard_tick`] calls. Default 100 ms
    /// (matching Redis's `hz = 10`). `0` disables ticking entirely.
    fn shard_tick_interval_ms(&self) -> u64 {
        100
    }

    /// Snapshot of the runtime-owned knobs that can be hot-modified
    /// (the kevy server wires this to `CONFIG SET`). Called once per
    /// shard tick — each `Some` value is applied to the shard's live
    /// state; each `None` keeps the existing setting untouched.
    ///
    /// Default returns all-None so embedders that never hot-swap config
    /// pay nothing beyond one struct-build per tick. The cost lives in
    /// the impl's read of its own config source.
    fn live_runtime_config(&self) -> LiveRuntimeConfig {
        LiveRuntimeConfig::default()
    }

    /// Classify a command for blocking semantics. `BlockHint::None`
    /// (default) is the zero-cost answer for every non-blocking verb;
    /// the dispatcher only registers a waiter when this returns
    /// `BlockHint::Block` *and* the command's `dispatch_into` produced no
    /// reply (i.e. it could not satisfy itself immediately — e.g. BLPOP
    /// on an empty list). Concrete impls should fold this into their
    /// override of [`Self::resolve`] so the verb-table lookup happens
    /// once per command.
    fn block_hint<A: ArgvView + ?Sized>(&self, _args: &A) -> BlockHint {
        BlockHint::None
    }

    /// Rewrite `args` into the owned [`Argv`] that the dispatcher will
    /// store as the parked waiter's command and replay on wake. Lets a
    /// command set normalise positional ID / cursor arguments that would
    /// otherwise re-resolve to a different value on retry — most notably
    /// `XREAD BLOCK ... STREAMS k $`, where leaving `$` literal in the
    /// retried argv causes a fresh re-resolve to the post-`XADD` last_id
    /// and zero matching entries (the wake hangs).
    ///
    /// Default: just materialise the argv unchanged. Concrete impls only
    /// need to override when a registered command carries an arg whose
    /// meaning depends on store state at park time (`XREAD $`, the
    /// classic case).
    ///
    /// For the cross-shard arbiter this runs on the **target** shard (the
    /// one that owns the key) when the waiter is armed, so `$` snapshots
    /// the target's real `last_id` — not the origin shard's (which may not
    /// hold the stream at all).
    fn resolve_block_argv<A: ArgvView + ?Sized>(
        &self,
        _store: &mut Store,
        args: &A,
        _kind: BlockKind,
    ) -> Argv {
        args.to_argv()
    }

    /// Build the **single-key** command the dispatcher will replay to
    /// satisfy one watched `key` of a (possibly multi-key) blocking
    /// command. `args` is the original command; `key` is one of its
    /// watched keys. Returns an [`Argv`] that, when dispatched, pops /
    /// reads only `key` — e.g. `BLPOP k1 k2 0` watching `k2` yields
    /// `BLPOP k2 0`; `XREAD … STREAMS s1 s2 id1 id2` watching `s2`
    /// yields `XREAD … STREAMS s2 id2`.
    ///
    /// Any state-dependent positional arg (`$`) is left **literal** here —
    /// it's frozen later by [`Self::resolve_block_argv`] on the key's
    /// owning shard. No store access needed (pure argv slicing). Default:
    /// the unchanged argv (single-key blocking commands need no rewrite).
    fn block_serve_argv<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        _kind: BlockKind,
        _key: &[u8],
    ) -> Argv {
        args.to_argv()
    }

    /// The command that would put back whatever replaying `serve_argv` is
    /// about to consume — read from the store **before** the serve runs.
    ///
    /// A cross-shard serve pops on the target and ships the reply to the
    /// origin. If the origin's client disconnected in that window the
    /// reply has nowhere to go, and the element would be lost: taken
    /// from the list, delivered to nobody. The origin cannot put it back
    /// (it holds a RESP frame whose shape differs per kind *and* per
    /// negotiated protocol), so the target captures the undo first and
    /// holds it until the origin confirms delivery.
    ///
    /// Read, not parse: the peek runs on the owning shard immediately
    /// before the pop with nothing interleaved, so what it saw is what
    /// the pop takes, in RESP2 and RESP3 alike.
    ///
    /// `None` = nothing to undo. That is the honest answer for kinds
    /// that consume nothing (`XREAD` is non-destructive) and the safe
    /// default for an embedder that has not implemented it.
    fn block_restore_argv(
        &self,
        _store: &mut Store,
        _kind: BlockKind,
        _key: &[u8],
    ) -> Option<Argv> {
        None
    }

    /// Non-destructive readiness peek for a parked waiter: would replaying
    /// `serve_argv` (built by [`Self::block_serve_argv`], `$` already
    /// frozen) produce a reply right now? Runs on the key's owning shard
    /// when arming and is the gate for emitting a cross-shard wake. Must
    /// NOT mutate the store (no pop / no group-cursor advance). Default
    /// `false` so non-blocking embedders never spuriously wake.
    fn block_ready<A: ArgvView + ?Sized>(
        &self,
        _store: &mut Store,
        _serve_argv: &A,
        _kind: BlockKind,
    ) -> bool {
        false
    }

    /// Validate a command being queued inside `MULTI`. Returns an error
    /// reply (already RESP-encoded, e.g. `-ERR unknown command …`) when
    /// the command cannot be queued — an unknown verb or an arity
    /// mismatch — in which case the caller answers with it instead of
    /// `+QUEUED` and marks the transaction dirty so `EXEC` aborts with
    /// `-EXECABORT`. `None` means "queue it". Default `None` keeps
    /// embedders that don't model a verb table permissive.
    fn queue_error<A: ArgvView + ?Sized>(&self, _args: &A) -> Option<Vec<u8>> {
        None
    }

    /// Resolve all verb-dependent attributes in **one** verb-table lookup.
    /// The default implementation calls the per-attribute methods above
    /// (five upper_verb scans + matches); concrete impls SHOULD override
    /// this with a single match so the reactor's hot path pays the verb-
    /// resolution cost only once per command.
    fn resolve<A: ArgvView + ?Sized>(&self, args: &A) -> ResolvedCmd {
        ResolvedCmd {
            txn_kind: self.txn_kind(args),
            route: self.route(args),
            is_quit: self.is_quit(args),
            is_write: self.is_write(args),
            block_hint: self.block_hint(args),
            wake_idx: None,
        }
    }
}
