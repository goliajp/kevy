//! The [`Commands`] trait — the seam between the runtime and a command
//! implementation. Split from `lib.rs` for the 500-LOC house rule.

use crate::{
    BlockHint, BlockKind, ExtensionReduced, GeoHits, LiveRuntimeConfig, NotifyKind, ReplicaViewRow,
    ResolvedCmd, Route, Store, TxnKind,
};
use kevy_resp::{Argv, ArgvView, RespVersion};

#[doc = include_str!("commands_docs/commands.md")]
pub trait Commands: Clone + Send + 'static {
    #[doc = include_str!("commands_docs/route.md")]
    fn route<A: ArgvView + ?Sized>(&self, args: &A) -> Route;
    #[doc = include_str!("commands_docs/dispatch.md")]
    fn dispatch<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A) -> Vec<u8>;
    #[doc = include_str!("commands_docs/dispatch_into.md")]
    fn dispatch_into<A: ArgvView + ?Sized>(&self, store: &mut Store, args: &A, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.dispatch(store, args));
    }
    #[doc = include_str!("commands_docs/dispatch_into_resp3.md")]
    fn dispatch_into_resp3<A: ArgvView + ?Sized>(
        &self,
        store: &mut Store,
        args: &A,
        out: &mut Vec<u8>,
    ) {
        self.dispatch_into(store, args, out);
    }
    #[doc = include_str!("commands_docs/dispatch_verb_into.md")]
    fn dispatch_verb_into<A: ArgvView + ?Sized>(
        &self,
        store: &mut Store,
        args: &A,
        _verb: crate::VerbId,
        proto: RespVersion,
        out: &mut Vec<u8>,
    ) {
        match proto {
            RespVersion::V2 => self.dispatch_into(store, args, out),
            RespVersion::V3 => self.dispatch_into_resp3(store, args, out),
        }
    }
    #[doc = include_str!("commands_docs/notify_class.md")]
    fn notify_class<A: ArgvView + ?Sized>(&self, _args: &A) -> Option<NotifyKind> {
        None
    }

    #[doc = include_str!("commands_docs/hello_reply.md")]
    fn hello_reply<A: ArgvView + ?Sized>(
        &self,
        _args: &A,
        current_proto: RespVersion,
    ) -> (RespVersion, Vec<u8>) {
        (current_proto, b"+OK\r\n".to_vec())
    }
    #[doc = include_str!("commands_docs/is_quit.md")]
    fn is_quit<A: ArgvView + ?Sized>(&self, args: &A) -> bool;
    #[doc = include_str!("commands_docs/is_write.md")]
    fn is_write<A: ArgvView + ?Sized>(&self, args: &A) -> bool;
    #[doc = include_str!("commands_docs/txn_kind.md")]
    fn txn_kind<A: ArgvView + ?Sized>(&self, args: &A) -> TxnKind;
    #[doc = include_str!("commands_docs/on_shard_init.md")]
    fn on_shard_init(&self, _store: &mut Store) {}

    #[doc = include_str!("commands_docs/on_shard_start.md")]
    fn on_shard_start(&self, _shard: usize) {}

    #[doc = include_str!("commands_docs/on_data_dir.md")]
    fn on_data_dir(&self, _dir: &std::path::Path) {}

    #[doc = include_str!("commands_docs/on_persist_stats.md")]
    fn on_persist_stats(&self, _in_flight: bool, _aof_rewrites_total: u64) {}

    #[doc = include_str!("commands_docs/on_tick_gap.md")]
    fn on_tick_gap(&self, _excess_us: u64) {}

    #[doc = include_str!("commands_docs/on_query_buffer_exceeded.md")]
    fn on_query_buffer_exceeded(&self) {}

    #[doc = include_str!("commands_docs/on_aof_format.md")]
    fn on_aof_format(&self, _format: u8) {}

    #[doc = include_str!("commands_docs/on_replay_report.md")]
    fn on_replay_report(&self, _dropped_bytes: u64, _corrupt: bool) {}

    #[doc = include_str!("commands_docs/on_conn_gauge.md")]
    fn on_conn_gauge(&self, _live: u64) {}

    /// Publish how many connections are parked in a blocking command on
    /// this shard, once per tick, beside [`Self::on_conn_gauge`].
    ///
    /// Defaulted to a no-op so adding it breaks no implementor.
    ///
    /// # Examples
    ///
    /// It is a gauge, not a counter: each tick replaces the value rather
    /// than adjusting it, so an implementation stores and never adds. A
    /// connection blocked on several keys at once is still one connection.
    ///
    /// ```
    /// use core::sync::atomic::{AtomicU64, Ordering};
    ///
    /// // What an implementation does with the argument.
    /// static BLOCKED: AtomicU64 = AtomicU64::new(0);
    /// let publish = |n: u64| BLOCKED.store(n, Ordering::Relaxed);
    ///
    /// publish(2);
    /// assert_eq!(BLOCKED.load(Ordering::Relaxed), 2);
    ///
    /// // One of them woke. The next tick replaces the reading; nothing
    /// // decrements, so a missed tick cannot leave the gauge drifting.
    /// publish(1);
    /// assert_eq!(BLOCKED.load(Ordering::Relaxed), 1);
    /// ```
    fn on_blocked_gauge(&self, _blocked: u64) {}

    #[doc = include_str!("commands_docs/on_replication_view.md")]
    fn on_replication_view(&self, _master_repl_offset: u64, _replicas: Vec<ReplicaViewRow>) {}

    #[doc = include_str!("commands_docs/on_shard_tick.md")]
    fn on_shard_tick(&self, _store: &mut Store) {}

    #[doc = include_str!("commands_docs/shutdown_save_requested.md")]
    fn shutdown_save_requested(&self) -> bool {
        false
    }

    #[doc = include_str!("commands_docs/extension_op.md")]
    fn extension_op(&self, _store: &mut Store, _argv: &[Vec<u8>]) -> Vec<u8> {
        Vec::new()
    }

    #[doc = include_str!("commands_docs/geo_search.md")]
    fn geo_search(&self, _store: &mut Store, _argv: &[Vec<u8>]) -> GeoHits {
        GeoHits::Error(b"-ERR unknown command\r\n".to_vec())
    }

    #[doc = include_str!("commands_docs/xreadgroup_refusal.md")]
    fn xreadgroup_refusal(&self, _store: &mut Store, _argv: &Argv) -> Option<Vec<u8>> {
        None
    }

    #[doc = include_str!("commands_docs/write_denied.md")]
    fn write_denied(&self) -> Option<Vec<u8>> {
        None
    }

    #[doc = include_str!("commands_docs/read_denied.md")]
    fn read_denied<A: ArgvView + ?Sized>(&self, _args: &A) -> Option<Vec<u8>> {
        None
    }

    #[doc = include_str!("commands_docs/extension_reduce.md")]
    fn extension_reduce(
        &self,
        _argv: &[Vec<u8>],
        _chunks: Vec<Vec<u8>>,
        _proto: kevy_resp::RespVersion,
    ) -> ExtensionReduced {
        ExtensionReduced::Reply(b"-ERR extension commands not supported\r\n".to_vec())
    }

    #[doc = include_str!("commands_docs/on_write.md")]
    fn on_write(&self, _store: &mut Store, _key: &[u8]) {}

    #[doc = include_str!("commands_docs/take_ext_out.md")]
    fn take_ext_out(&self) -> Vec<(usize, Vec<u8>)> {
        Vec::new()
    }

    #[doc = include_str!("commands_docs/apply_ext.md")]
    fn apply_ext(&self, _store: &mut Store, _payload: &[u8]) {}

    #[doc = include_str!("commands_docs/extension_targets.md")]
    fn extension_targets(&self, _argv: &[Vec<u8>]) -> Option<Vec<usize>> {
        None
    }

    #[doc = include_str!("commands_docs/on_flush.md")]
    fn on_flush(&self, _store: &mut Store) {}

    #[doc = include_str!("commands_docs/snapshot_aux.md")]
    fn snapshot_aux(&self) -> Option<Argv> {
        None
    }

    #[doc = include_str!("commands_docs/load_snapshot_aux.md")]
    fn load_snapshot_aux(&self, _frame: Option<&Argv>, _full_sync: bool) {}

    #[doc = include_str!("commands_docs/on_restored.md")]
    fn on_restored(&self, _record: &mut dyn FnMut(&Argv) -> bool) {}

    #[doc = include_str!("commands_docs/on_command.md")]
    fn on_command(&self) {}

    #[doc = include_str!("commands_docs/on_connection.md")]
    fn on_connection(&self) {}

    #[doc = include_str!("commands_docs/shard_tick_interval_ms.md")]
    fn shard_tick_interval_ms(&self) -> u64 {
        100
    }

    #[doc = include_str!("commands_docs/live_runtime_config.md")]
    fn live_runtime_config(&self) -> LiveRuntimeConfig {
        LiveRuntimeConfig::default()
    }

    #[doc = include_str!("commands_docs/block_hint.md")]
    fn block_hint<A: ArgvView + ?Sized>(&self, _args: &A) -> BlockHint {
        BlockHint::None
    }

    #[doc = include_str!("commands_docs/resolve_block_argv.md")]
    fn resolve_block_argv<A: ArgvView + ?Sized>(
        &self,
        _store: &mut Store,
        args: &A,
        _kind: BlockKind,
    ) -> Argv {
        args.to_argv()
    }

    #[doc = include_str!("commands_docs/block_serve_argv.md")]
    fn block_serve_argv<A: ArgvView + ?Sized>(
        &self,
        args: &A,
        _kind: BlockKind,
        _key: &[u8],
    ) -> Argv {
        args.to_argv()
    }

    #[doc = include_str!("commands_docs/block_restore_argv.md")]
    fn block_restore_argv(
        &self,
        _store: &mut Store,
        _kind: BlockKind,
        _key: &[u8],
    ) -> Option<Argv> {
        None
    }

    #[doc = include_str!("commands_docs/block_ready.md")]
    fn block_ready<A: ArgvView + ?Sized>(
        &self,
        _store: &mut Store,
        _serve_argv: &A,
        _kind: BlockKind,
    ) -> bool {
        false
    }

    #[doc = include_str!("commands_docs/queue_error.md")]
    fn queue_error<A: ArgvView + ?Sized>(&self, _args: &A) -> Option<Vec<u8>> {
        None
    }

    #[doc = include_str!("commands_docs/resolve.md")]
    fn resolve<A: ArgvView + ?Sized>(&self, args: &A) -> ResolvedCmd {
        ResolvedCmd {
            txn_kind: self.txn_kind(args),
            route: self.route(args),
            is_quit: self.is_quit(args),
            is_write: self.is_write(args),
            block_hint: self.block_hint(args),
            wake_idx: None,
            verb: crate::VerbId::UNKNOWN,
        }
    }
}
