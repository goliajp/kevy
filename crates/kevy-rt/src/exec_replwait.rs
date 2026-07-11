//! Replication barriers — the deferred all-shard waiters behind
//! `WAIT numreplicas timeout` (D1) and `REPL.WAIT token… [TIMEOUT ms]`
//! (D2).
//!
//! Shape: the origin shard registers ONE pending slot (`Agg::MinInt` /
//! `Agg::ReplBarrier`) with `remaining == nshards`, then arms every
//! shard. A shard whose condition already holds answers immediately;
//! otherwise it parks a [`ReplWaiter`] and answers later — from the
//! ACK-parse path (D1: a replica's `REPLCONF ACK` advanced its slot),
//! from the replica-apply drain (D2: the apply position moved), or
//! from the reactor's timeout tick. Answers ride
//! [`Inbound::ReplDone`], NOT `Inbound::Response`: they may land
//! seconds later and must not touch `xshard_inflight` (which would pin
//! the origin core in the busy-poll rung for the whole wait).
//!
//! Wake truth:
//! - D1 counts a replica as "acked" when a LIVE replication conn's
//!   slot has `acked_offset ≥` the shard's `master_repl_offset` frozen
//!   at arm receipt (`SlotTable` is the ack source of truth; liveness
//!   filters out a recently-dead replica's residual slot).
//! - D2 compares the shard's own replication-apply position
//!   (`Shard::replica_applied_next`, advanced on the reactor thread as
//!   frames apply) — NOT the runner's ACK cursor, which counts frames
//!   *enqueued* into the shard inbox and would let a REPL.WAIT `+OK`
//!   race ahead of a subsequent GET (the read-your-writes hole).

use crate::Commands;
use crate::blocked::unix_now_ms;
use crate::message::{Agg, Inbound, Part};
use crate::replication::ReplicaState;
use crate::shard::Shard;

/// Hard cap on any WAIT / REPL.WAIT deadline. Redis's `WAIT n 0`
/// blocks forever; kevy converts "forever" to this bound so a parked
/// waiter can never outlive an operator's patience (documented in the
/// verb reference).
pub(crate) const WAIT_HARD_CAP_MS: u64 = 60_000;

/// One parked barrier participant on this (target) shard.
pub(crate) struct ReplWaiter {
    /// Shard that owns the client conn (folds the answer).
    origin: usize,
    conn: u64,
    seq: u64,
    /// Unix-ms wall clock when the shard must answer with its current
    /// state (same clock as the BLPOP timeout sweep).
    deadline_ms: u64,
    kind: ReplWaiterKind,
}

#[derive(Clone, Copy)]
pub(crate) enum ReplWaiterKind {
    /// D1 `WAIT`: answer the acked-replica count once it reaches
    /// `need` (target = this shard's `master_repl_offset` at arm).
    AckCount { target_offset: u64, need: u32 },
    /// D2 `REPL.WAIT`: answer 1 once `replica_applied_next ≥
    /// min_offset` (0 on deadline).
    Applied { min_offset: u64 },
}

/// Clamp a client-supplied timeout to the parked-waiter hard cap.
/// `0` is the Redis "wait forever" spelling → the full cap.
fn effective_deadline(timeout_ms: u64) -> u64 {
    let t = if timeout_ms == 0 { WAIT_HARD_CAP_MS } else { timeout_ms.min(WAIT_HARD_CAP_MS) };
    unix_now_ms().saturating_add(t)
}

impl<C: Commands> Shard<C> {
    /// Origin side of `WAIT numreplicas timeout` — one pending slot
    /// folding MIN over every shard's acked-replica count.
    pub(crate) fn start_repl_wait(
        &mut self,
        conn_id: u64,
        seq: u64,
        need: u32,
        timeout_ms: u64,
    ) {
        let deadline_ms = effective_deadline(timeout_ms);
        // Ship any batched single-key forwards BEFORE arming: a
        // pipelined `SET k v; WAIT …` parks the SET in
        // `request_batch[owner]` until the loop-end flush, while
        // `send_to` puts the arm on the ring NOW — unflushed, the arm
        // would overtake the write and the target would freeze its
        // barrier offset WITHOUT it. The ring is FIFO per shard pair,
        // so flushing first restores program order.
        self.flush_requests();
        self.push_pending_slot(conn_id, self.nshards as u32, Agg::MinInt(i64::MAX), false);
        let me = self.id;
        for s in 0..self.nshards {
            if s == me {
                self.arm_repl_wait(me, conn_id, seq, need, deadline_ms);
            } else {
                self.send_to(
                    s,
                    Inbound::ReplWaitArm { origin: me, conn: conn_id, seq, need, deadline_ms },
                );
            }
        }
    }

    /// Origin side of `REPL.WAIT` — one pending slot folding the
    /// all-shards-met barrier; `miss` is the pre-built reply for any
    /// timeout (the command layer's `-MISDIRECTED writer is …`).
    pub(crate) fn start_repl_barrier(
        &mut self,
        conn_id: u64,
        seq: u64,
        offsets: Vec<u64>,
        timeout_ms: u64,
        miss: Vec<u8>,
    ) {
        if offsets.len() != self.nshards {
            let err = format!(
                "-ERR REPL.WAIT token has {} (gen, offset) pair(s) but this server has {} \
                 shard(s); take the token from this server's primary with REPL.TOKEN\r\n",
                offsets.len(),
                self.nshards,
            );
            self.push_pending_slot(conn_id, 1, Agg::First(None), false);
            self.fold(
                conn_id,
                seq,
                Part::Reply(crate::message::SmallReply::from_vec(err.into_bytes())),
            );
            return;
        }
        let deadline_ms = effective_deadline(timeout_ms);
        self.push_pending_slot(
            conn_id,
            self.nshards as u32,
            Agg::ReplBarrier { ok: true, miss },
            false,
        );
        let me = self.id;
        for (s, min_offset) in offsets.into_iter().enumerate() {
            if s == me {
                self.arm_repl_apply(me, conn_id, seq, min_offset, deadline_ms);
            } else {
                self.send_to(
                    s,
                    Inbound::ReplApplyArm {
                        origin: me,
                        conn: conn_id,
                        seq,
                        min_offset,
                        deadline_ms,
                    },
                );
            }
        }
    }

    /// Target side of one WAIT participant: answer now if `need`
    /// replicas already acked this shard's current offset, else park.
    pub(crate) fn arm_repl_wait(
        &mut self,
        origin: usize,
        conn: u64,
        seq: u64,
        need: u32,
        deadline_ms: u64,
    ) {
        let target_offset = self
            .replicate
            .as_ref()
            .map_or(0, |f| f.source().next_offset());
        let n = self.repl_ack_count(target_offset);
        if n >= i64::from(need) {
            self.repl_waiter_reply(origin, conn, seq, n);
            return;
        }
        self.repl_waiters.push(ReplWaiter {
            origin,
            conn,
            seq,
            deadline_ms,
            kind: ReplWaiterKind::AckCount { target_offset, need },
        });
    }

    /// Target side of one REPL.WAIT participant: answer 1 now if the
    /// apply position already covers `min_offset`, else park.
    pub(crate) fn arm_repl_apply(
        &mut self,
        origin: usize,
        conn: u64,
        seq: u64,
        min_offset: u64,
        deadline_ms: u64,
    ) {
        if self.replica_applied_next >= min_offset {
            self.repl_waiter_reply(origin, conn, seq, 1);
            return;
        }
        self.repl_waiters.push(ReplWaiter {
            origin,
            conn,
            seq,
            deadline_ms,
            kind: ReplWaiterKind::Applied { min_offset },
        });
    }

    /// Replicas that have acked `target_offset`: slots (the ack truth)
    /// filtered to ids with a LIVE handshake-complete conn, so a
    /// recently-dead replica's residual slot (reconnect window) never
    /// counts toward WAIT.
    fn repl_ack_count(&self, target_offset: u64) -> i64 {
        self.slots
            .iter()
            .filter(|s| s.acked_offset >= target_offset && self.replica_conn_is_live(&s.id))
            .count() as i64
    }

    fn replica_conn_is_live(&self, id: &str) -> bool {
        self.replicas.iter().any(|c| match &c.state {
            ReplicaState::AckSent { replica_id, .. }
            | ReplicaState::Streaming { replica_id, .. }
            | ReplicaState::SnapshotShipping { replica_id, .. } => replica_id == id,
            _ => false,
        })
    }

    /// Re-check parked D1 waiters. Called after `parse_replica_acks`
    /// advanced a slot; cheap no-op (one `is_empty`) otherwise.
    pub(crate) fn check_repl_ack_waiters(&mut self) {
        if self.repl_waiters.is_empty() {
            return;
        }
        let mut done: Vec<(usize, u64, u64, i64)> = Vec::new();
        {
            // Disjoint field borrows: the retain closure reads the ack
            // truth (slots + live conns) while mutating the waiter Vec.
            let slots = &self.slots;
            let replicas = &self.replicas;
            self.repl_waiters.retain(|w| {
                if let ReplWaiterKind::AckCount { target_offset, need } = w.kind {
                    let n = slots
                        .iter()
                        .filter(|s| {
                            s.acked_offset >= target_offset
                                && replicas
                                    .iter()
                                    .any(|c| replica_conn_id(c) == Some(s.id.as_str()))
                        })
                        .count() as i64;
                    if n >= i64::from(need) {
                        done.push((w.origin, w.conn, w.seq, n));
                        return false;
                    }
                }
                true
            });
        }
        for (origin, conn, seq, n) in done {
            self.repl_waiter_reply(origin, conn, seq, n);
        }
    }

    /// Re-check parked D2 waiters. Called after the replica-apply
    /// drain moved `replica_applied_next`.
    pub(crate) fn check_repl_apply_waiters(&mut self) {
        if self.repl_waiters.is_empty() {
            return;
        }
        let applied = self.replica_applied_next;
        let mut done: Vec<(usize, u64, u64)> = Vec::new();
        self.repl_waiters.retain(|w| {
            if let ReplWaiterKind::Applied { min_offset } = w.kind
                && applied >= min_offset
            {
                done.push((w.origin, w.conn, w.seq));
                return false;
            }
            true
        });
        for (origin, conn, seq) in done {
            self.repl_waiter_reply(origin, conn, seq, 1);
        }
    }

    /// Deadline sweep — every expired waiter answers its CURRENT state
    /// (D1: the achieved count; D2: 0 = barrier missed). Runs on the
    /// same reactor cadence as `tick_blocked_timeouts`.
    pub(crate) fn tick_repl_waiters(&mut self) {
        if self.repl_waiters.is_empty() {
            return;
        }
        let now_ms = unix_now_ms();
        let waiters = std::mem::take(&mut self.repl_waiters);
        let (expired, keep): (Vec<_>, Vec<_>) =
            waiters.into_iter().partition(|w| w.deadline_ms <= now_ms);
        self.repl_waiters = keep;
        for w in expired {
            let n = match w.kind {
                ReplWaiterKind::AckCount { target_offset, .. } => {
                    self.repl_ack_count(target_offset)
                }
                ReplWaiterKind::Applied { .. } => 0,
            };
            self.repl_waiter_reply(w.origin, w.conn, w.seq, n);
        }
    }

    /// Deliver one shard's answer: fold locally when this shard IS the
    /// origin (then flag the conn for the reactor's flush/arm pass —
    /// fold alone leaves the bytes parked in `conn.output`), else ship
    /// an [`Inbound::ReplDone`] home.
    pub(crate) fn repl_waiter_reply(&mut self, origin: usize, conn: u64, seq: u64, n: i64) {
        if origin == self.id {
            self.fold(conn, seq, Part::Int(n));
            self.mark_pending_write_dirty(conn);
        } else {
            self.send_to(origin, Inbound::ReplDone { conn, seq, n });
        }
    }
}

/// The replica id of a handshake-complete conn (`None` pre-handshake
/// / closed). Free fn so `check_repl_ack_waiters` can use it inside a
/// `retain` closure that already borrows `self.repl_waiters`.
fn replica_conn_id(c: &crate::replication::ReplicaConn) -> Option<&str> {
    match &c.state {
        ReplicaState::AckSent { replica_id, .. }
        | ReplicaState::Streaming { replica_id, .. }
        | ReplicaState::SnapshotShipping { replica_id, .. } => Some(replica_id),
        _ => None,
    }
}
