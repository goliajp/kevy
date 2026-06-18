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
        if let Some(ms) = live.tick_interval_ms {
            *tick_interval = if ms == 0 {
                None
            } else {
                Some(Duration::from_millis(ms))
            };
        }
        if let Some(flags) = live.notify_flags {
            self.notify_flags = flags;
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
    }

    /// Check whether the live AOF has grown enough to warrant an automatic
    /// `BGREWRITEAOF`, and run it inline if so. Called from the tick path
    /// — at most every `tick_interval_ms`, so the cost is amortised across
    /// thousands of writes per check. No-op when AOF is disabled, when the
    /// `auto_aof_rewrite_pct` knob is `0`, or when the current AOF is
    /// smaller than `auto_aof_rewrite_min_size`.
    pub(crate) fn maybe_auto_rewrite_aof(&mut self) {
        if self.auto_aof_rewrite_pct == 0 {
            return;
        }
        let Some(aof) = &self.aof else { return };
        let cur = aof.size_bytes();
        if cur < self.auto_aof_rewrite_min_size {
            return;
        }
        let baseline = aof.size_at_last_rewrite().max(1);
        // (cur - baseline) * 100 / baseline ≥ pct  ⇔  cur * 100 ≥ baseline * (100 + pct)
        let lhs = cur.saturating_mul(100);
        let rhs = baseline.saturating_mul(100u64.saturating_add(u64::from(self.auto_aof_rewrite_pct)));
        if lhs < rhs {
            return;
        }
        self.start_bg_rewrite();
    }

    /// Tick half of background persistence: apply any finished BGSAVE /
    /// rewrite (commit or abort — see `poll_persist_done`), then check the
    /// auto-rewrite threshold.
    pub(crate) fn tick_persist(&mut self) {
        self.poll_persist_done();
        self.maybe_auto_rewrite_aof();
        let in_flight =
            self.persist.busy() || self.aof.as_ref().is_some_and(kevy_persist::Aof::is_rewriting);
        let rewrites = self.aof.as_ref().map_or(0, kevy_persist::Aof::rewrites_total);
        self.commands.on_persist_stats(in_flight, rewrites);
    }

    /// Publish this shard's replication view (master offset + connected
    /// replicas count) to the embedder. No-op when replication is off
    /// (the standalone fast path: one Option-discriminant check + an
    /// early return). Same per-tick cadence as
    /// [`Self::tick_persist`]; the command layer that serves `ROLE` /
    /// `INFO replication` reads from the thread-local the embedder
    /// stashes in [`crate::Commands::on_replication_view`].
    /// T1.22.5: compute the per-shard backlog retention watermark
    /// — `min(live sent_offsets, slot.min_acked_offset)` — and tell
    /// the source to drop frames every consumer has moved past.
    /// No-op when no consumer position exists yet (cold startup,
    /// no replicas / no slots) so a brand-new replica still finds
    /// the full backlog. Pure win on the steady-state: a slow
    /// replica can pin retention via its `sent_offset`, but
    /// fast/closed replicas no longer hold bytes the slow one is
    /// catching up to.
    pub(crate) fn tick_replication_watermark(&mut self) {
        let Some(src) = self.replicate.as_mut() else { return };
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
        let Some(src) = &self.replicate else { return };
        let offset = src.next_offset();
        // Collect per-replica `(ipv4, port, sent_offset)` from every
        // handshake-complete replica conn. `peer` was captured at
        // accept time (T1.28.5); `sent_offset` is the live value
        // from the state machine. For `SnapshotShipping`, report
        // `ack_offset` (the snapshot's frozen-at offset) since
        // streaming hasn't started yet.
        let mut replicas = Vec::with_capacity(self.replicas.len());
        for c in &self.replicas {
            let sent = match &c.state {
                ReplicaState::AckSent { from_offset, .. } => *from_offset,
                ReplicaState::Streaming { sent_offset, .. } => *sent_offset,
                ReplicaState::SnapshotShipping { ack_offset, .. } => *ack_offset,
                _ => continue,
            };
            replicas.push((c.peer.0, c.peer.1, sent));
        }
        self.commands.on_replication_view(offset, replicas);
    }
}
