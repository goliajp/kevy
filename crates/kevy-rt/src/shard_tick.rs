//! Per-tick housekeeping for the [`Shard`] reactor — pulled out of
//! [`crate::shard`] to keep that file under the 500-LOC house rule.
//!
//! Called from the reactor's tick branch (once per `tick_interval_ms`,
//! 100 ms by default). Each `Some` value from the embedder's
//! [`crate::Commands::live_runtime_config`] tick is applied to the
//! shard's live state, and the auto-AOF-rewrite check fires if the
//! live AOF has grown past its threshold.

use crate::Commands;
use crate::replication::ReplicaState;
use crate::shard::Shard;
use std::time::Duration;

impl<C: Commands> Shard<C> {
    /// Pull the live runtime knobs from the [`crate::Commands`] impl
    /// and apply each `Some` to the shard's state. Called from the
    /// tick branch (once per `tick_interval_ms`) so the cost is
    /// amortised across thousands of commands; embedders that never
    /// hot-swap inherit the trait default (all-None → zero work
    /// beyond one struct build).
    pub(crate) fn apply_live_runtime_config(&mut self, tick_interval: &mut Option<Duration>) {
        let live = self.commands.live_runtime_config();
        self.apply_live_persist_knobs(&live);
        if let Some(ms) = live.tick_interval_ms {
            *tick_interval = if ms == 0 {
                None
            } else {
                Some(Duration::from_millis(ms))
            };
        }
        if let Some(flags) = live.notify_flags {
            self.notify_flags = flags;
            // Mirror the store-origin event classes into the store's
            // capture mask (all-off keeps every store hook at a single
            // byte test). Channel gating still happens at publish time.
            let on = !flags.is_empty();
            self.store.set_notify_capture(
                on && flags.new_key,
                on && flags.expired,
                on && flags.evicted,
            );
        }
        if let Some(t) = live.slowlog_slower_than_micros {
            self.slowlog.slower_than_micros = t;
        }
        if let Some(n) = live.slowlog_max_len {
            self.slowlog.max_len = n;
            let cap = n as usize;
            while self.slowlog.buf.len() > cap {
                self.slowlog.buf.pop_front();
            }
        }
        self.apply_promotion_epoch(live.promotion_epoch);
    }

    /// The persistence half of [`Self::apply_live_runtime_config`]:
    /// fsync policy + the three rewrite triggers.
    fn apply_live_persist_knobs(&mut self, live: &crate::LiveRuntimeConfig) {
        if let Some(f) = live.appendfsync
            && let Some(aof) = &mut self.aof
        {
            // A failure to flush on policy tighten is logged but doesn't
            // bring the shard down — the policy itself still takes effect
            // and subsequent appends will retry the sync.
            if let Err(e) = aof.set_fsync(f) {
                eprintln!("kevy: shard {} set_fsync failed: {e}", self.id);
            }
        }
        if let Some(p) = live.auto_aof_rewrite_pct {
            self.auto_aof_rewrite_pct = p;
        }
        if let Some(m) = live.auto_aof_rewrite_min_size {
            self.auto_aof_rewrite_min_size = m;
        }
        if let Some(b) = live.auto_aof_rewrite_bytes {
            self.auto_aof_rewrite_bytes = b;
        }
        if let Some(i) = live.auto_aof_rewrite_interval_secs {
            self.auto_aof_rewrite_interval_secs = i;
        }
    }

    /// Promotion fences the old offset space: when the
    /// command layer's promotion counter moved (this process went
    /// replica → primary), bump this shard's feed generation (offsets
    /// restart at 0, persisted via the feed-gen sidecar). Tokens
    /// minted pre-failover then gen-mismatch on every replica instead
    /// of falsely matching the new primary's unrelated offsets. The
    /// FIRST observed value is recorded without acting — a counter
    /// left over from an earlier in-process serve session must not
    /// fire a spurious bump at boot.
    fn apply_promotion_epoch(&mut self, epoch: u64) {
        let Some(seen) = self.seen_promotion_epoch else {
            self.seen_promotion_epoch = Some(epoch);
            return;
        };
        if epoch <= seen {
            return;
        }
        self.seen_promotion_epoch = Some(epoch);
        if let Some(f) = self.replicate.as_mut() {
            f.bump_generation();
            let g = f.generation();
            if let Err(e) = kevy_persist::feed_meta::write_feed_gen(&self.data_dir, self.id, g) {
                eprintln!("kevy: shard {} promotion feed gen write failed: {e}", self.id);
            }
            eprintln!(
                "kevy: shard {} promoted — replication feed generation bumped to {g}",
                self.id,
            );
        }
    }

    /// Check whether the live AOF is due for an automatic `BGREWRITEAOF`
    /// under the three-trigger [`kevy_persist::RewritePolicy`] (growth,
    /// absolute cap, staleness — the same decision the embedded reaper
    /// uses), and run it inline if so. Called from the tick path — at most
    /// every `tick_interval_ms`, so the cost is amortised across thousands
    /// of writes per check. No-op when AOF is disabled or all rules are 0.
    pub(crate) fn maybe_auto_rewrite_aof(&mut self) {
        let policy = kevy_persist::RewritePolicy {
            pct: self.auto_aof_rewrite_pct,
            min_size: self.auto_aof_rewrite_min_size,
            bytes: self.auto_aof_rewrite_bytes,
            interval_secs: self.auto_aof_rewrite_interval_secs,
        };
        let Some(aof) = &self.aof else { return };
        if !aof.rewrite_due(policy) {
            return;
        }
        self.start_bg_rewrite();
    }

    /// Tick half of background persistence: apply any finished BGSAVE /
    /// rewrite (commit or abort — see `poll_persist_done`), then check the
    /// auto-rewrite threshold.
    pub(crate) fn tick_persist(&mut self) {
        self.poll_persist_done();
        self.check_tee_overrun();
        self.maybe_auto_rewrite_aof();
        let in_flight =
            self.persist.busy() || self.aof.as_ref().is_some_and(kevy_persist::Aof::is_rewriting);
        let rewrites = self.aof.as_ref().map_or(0, kevy_persist::Aof::rewrites_total);
        self.commands.on_persist_stats(in_flight, rewrites);
        self.commands.on_aof_format(match self.aof.as_ref().map(kevy_persist::Aof::format) {
            None => 0,
            Some(kevy_persist::AofFormat::V1) => 1,
            Some(kevy_persist::AofFormat::V2) => 2,
        });
    }

    /// Publish this shard's live client-conn count (cluster-bus links
    /// excluded) — the `INFO connected_clients` truth source. Same
    /// per-tick cadence as [`Self::tick_persist`].
    pub(crate) fn tick_conn_gauge(&mut self) {
        let live = self.conns.iter().filter(|(_, c)| !c.cluster).count() as u64;
        self.commands.on_conn_gauge(live);
    }

    /// Disconnect any conn whose pending reply buffer has grown past
    /// [`crate::CLIENT_OUTPUT_HARD_LIMIT`] — a client that stopped
    /// reading (or a slow pub/sub subscriber) would otherwise let the
    /// per-conn `output` grow without bound and OOM the shard. The cap
    /// is on ACCUMULATED unflushed bytes, so a legitimate large reply
    /// (which drains progressively) never trips it; only a reader that
    /// isn't draining does. Async sweep (per-tick), matching Redis's
    /// out-of-band `client-output-buffer-limit` enforcement rather
    /// than a hot-path check. Epoll backend; the io_uring twin is
    /// [`Self::uring_enforce_output_limit`].
    pub(crate) fn enforce_output_limit(&mut self) {
        let mut over: Vec<u64> = Vec::new();
        for (id, c) in self.conns.iter() {
            if c.closing {
                continue;
            }
            let arc_bytes: usize = c.output_arcs.iter().map(|(_, a)| a.len()).sum();
            if c.output.len().saturating_add(arc_bytes) > crate::CLIENT_OUTPUT_HARD_LIMIT {
                over.push(*id);
            }
        }
        for id in over {
            eprintln!(
                "kevy: shard {} closing conn {id}: output buffer exceeded {} bytes",
                self.id,
                crate::CLIENT_OUTPUT_HARD_LIMIT,
            );
            if let Some(c) = self.conns.get_mut(&id) {
                c.closing = true;
            }
            self.dirty.push(id);
        }
    }

    /// Publish this shard's replication view (master offset + connected
    /// replicas count) to the embedder. No-op when replication is off
    /// (the standalone fast path: one Option-discriminant check + an
    /// early return). Same per-tick cadence as
    /// [`Self::tick_persist`]; the command layer that serves `ROLE` /
    /// `INFO replication` reads from the thread-local the embedder
    /// stashes in [`crate::Commands::on_replication_view`].
    /// Watermark: compute the per-shard backlog retention watermark
    /// — `min(live sent_offsets, slot.min_acked_offset)` — and tell
    /// the source to drop frames every consumer has moved past.
    /// No-op when no consumer position exists yet (cold startup,
    /// no replicas / no slots) so a brand-new replica still finds
    /// the full backlog. Pure win on the steady-state: a slow
    /// replica can pin retention via its `sent_offset`, but
    /// fast/closed replicas no longer hold bytes the slow one is
    /// catching up to.
    pub(crate) fn tick_replication_watermark(&mut self) {
        let Some(src) = self.replicate.as_mut().map(|f| f.source_mut()) else { return };
        let mut watermark: Option<u64> = None;
        for c in &self.replicas {
            let off = match &c.state {
                crate::replication::ReplicaState::AckSent { from_offset, .. } => *from_offset,
                crate::replication::ReplicaState::Streaming { sent_offset, .. } => *sent_offset,
                crate::replication::ReplicaState::SnapshotShipping { ack_offset, .. } => *ack_offset,
                _ => continue,
            };
            watermark = Some(watermark.map_or(off, |w| w.min(off)));
        }
        if let Some(slot_min) = self.slots.min_acked_offset() {
            watermark = Some(watermark.map_or(slot_min, |w| w.min(slot_min)));
        }
        if let Some(w) = watermark {
            src.drop_up_to(w);
        }
    }

    pub(crate) fn tick_replication_view(&mut self) {
        let Some(src) = self.replicate.as_ref().map(|f| f.source()) else { return };
        let offset = src.next_offset();
        // Collect per-replica `(ipv4, port, sent_offset)` from every
        // handshake-complete replica conn. `peer` was captured at
        // accept time; `sent_offset` is the live value
        // from the state machine. For `SnapshotShipping`, report
        // `ack_offset` (the snapshot's frozen-at offset) since
        // streaming hasn't started yet.
        let now_ns = std::time::Instant::now()
            .duration_since(self.replication_epoch)
            .as_nanos() as u64;
        let mut replicas = Vec::with_capacity(self.replicas.len());
        for c in &self.replicas {
            let (sent, id) = match &c.state {
                ReplicaState::AckSent { from_offset, replica_id, .. } => {
                    (*from_offset, replica_id.as_str())
                }
                ReplicaState::Streaming { sent_offset, replica_id, .. } => {
                    (*sent_offset, replica_id.as_str())
                }
                ReplicaState::SnapshotShipping { ack_offset, replica_id, .. } => {
                    (*ack_offset, replica_id.as_str())
                }
                _ => continue,
            };
            // The replica's ACKED offset from the slot table
            // (0 until its first REPLCONF ACK lands).
            // None = never ACKed; Some(0) is a REAL ack from an empty
            // replica's heartbeat round trip (min-replicas counts it).
            // The ACK age (vs the same epoch clock the slot was
            // touched with) feeds the min_replicas_max_lag_ms gate.
            let acked = self.slots.get(id).map(|s| crate::ReplicaAck {
                acked_offset: s.acked_offset,
                ack_age_ms: now_ns.saturating_sub(s.last_seen_ns) / 1_000_000,
            });
            replicas.push((id.to_string(), c.peer.0, c.peer.1, sent, acked));
        }
        self.commands.on_replication_view(offset, replicas);
    }

    /// Replication housekeeping for the io_uring path: it can't watch
    /// the replication listener / replica fds via epoll, so poll them
    /// once per tick (10 Hz). New replica accepts see ≤ 100 ms wait;
    /// handshake bytes ditto. The streaming pump stays per-iter via
    /// `pump_replication` — the throughput-sensitive write side.
    #[cfg(target_os = "linux")]
    pub(crate) fn uring_tick_replication(&mut self, now: std::time::Instant) {
        if let Err(e) = self.accept_ready_replication() {
            eprintln!("kevy: shard {} accept_ready_replication: {e}", self.id);
        }
        for idx in 0..self.replicas.len() {
            if let Err(e) = self.replica_readable(idx) {
                self.replica_io_failed(idx, "read", &e);
            }
            if let Err(e) = self.replica_writable(idx) {
                self.replica_io_failed(idx, "write", &e);
            }
        }
        self.tick_replication_slots(now);
        self.tick_replication_view();
        self.tick_replication_watermark();
    }
}
