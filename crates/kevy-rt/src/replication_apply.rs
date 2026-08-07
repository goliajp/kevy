//! Replica-side apply path — the [`Shard`] half of
//! the cross-thread bridge from the replica runner to the local
//! `Store`. The runner thread runs blocking
//! `kevy_replicate::replica::ReplicaClient::next_event` reads and
//! drops each event into the per-shard [`crate::ReplicaInboxSender`];
//! once per reactor tick, [`Shard::drain_replica_inbox`] empties the
//! channel and applies each event on the reactor's own thread (so
//! the `Store` is touched only by its owner, never cross-thread).
//!
//! Snapshot path: accumulates [`ReplicaApply::SnapshotChunk`] bytes
//! in `Shard.replica_snapshot_buf` until [`ReplicaApply::SnapshotEnd`]
//! arrives, then hands the buffer to `kevy_persist::load_snapshot_from`
//! → the local `Store` is replaced.
//!
//! Live-frame path: each [`ReplicaApply::Frame`] runs through
//! `Commands::dispatch_into` inside a [`crate::ReplicatedApplyGuard`]
//! scope (so the apply doesn't re-push into this shard's downstream
//! source) followed by the usual `post_write_housekeeping` (AOF /
//! WATCH bump / keyspace notify / BLOCK wake all still fire — local
//! readers must see consistent state).

use std::io::Cursor;

use crate::Commands;
use crate::message::DispatchMeta;
use crate::replica_inbox::ReplicaApply;
use crate::replication_gate::ReplicatedApplyGuard;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Drain every pending replica-runner event for this shard,
    /// applying each on the reactor thread. Called from the per-tick
    /// housekeeping branch alongside [`Self::tick_persist`]. No-op
    /// (one `Option::is_none` check) when this shard isn't running
    /// as a replica.
    pub(crate) fn drain_replica_inbox(&mut self) {
        let Some(inbox) = self.replica_inbox.as_ref() else {
            return;
        };
        // The wake contract says every send raises this flag, so a
        // lowered flag means an empty channel — return before touching
        // it. This line is load-bearing for throughput, not style: the
        // reactor calls this every iteration, and the unconditional
        // body (one Vec allocation + an idle mpsc probe, millions of
        // times a second across shards) took 27 % of ALL L1 misses in
        // a sadd A/B — the channel-state atomics of the eight shards
        // sat densely packed on shared cache lines and ping-ponged
        // (finding 2026-08-07-collection-tax-is-l1-misses).
        if !inbox.signal.wake_pending.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        // Lower the wake flag BEFORE reading: a send racing this drain
        // either lands in the try_iter below or re-raises the flag and
        // wakes the next `Poller::wait`. (Lowering after would let a
        // frame slip in between and go unannounced.)
        inbox.signal.wake_pending.store(false, std::sync::atomic::Ordering::Release);
        // Take ownership of all currently-queued events without
        // blocking. `try_iter` yields until the channel is empty;
        // we cap the per-call budget to keep the reactor responsive
        // when a flood of frames lands at once.
        const MAX_PER_TICK: usize = 1024;
        let mut events = Vec::with_capacity(64);
        for ev in inbox.inner.try_iter().take(MAX_PER_TICK) {
            events.push(ev);
        }
        // A capped drain leaves frames queued with the flag already
        // lowered, and the gate above would then sleep on them — the
        // remainder must re-raise its own flag.
        if events.len() == MAX_PER_TICK {
            inbox.signal.wake_pending.store(true, std::sync::atomic::Ordering::Release);
        }
        let applied_any = !events.is_empty();
        for ev in events {
            self.apply_replica_event(ev);
        }
        // The apply position moved — the REPL.WAIT wake
        // point. Answering HERE (after the store mutation, on the
        // reactor thread) is what makes `+OK` → GET read-your-writes:
        // by the time the reply leaves, this shard has applied
        // everything the token covers.
        if applied_any {
            self.check_repl_apply_waiters();
        }
    }

    /// Apply one [`ReplicaApply`] event. Split out so the iter
    /// borrow on `self.replica_inbox.inner` doesn't conflict with the
    /// `&mut self` apply methods need.
    fn apply_replica_event(&mut self, ev: ReplicaApply) {
        match ev {
            ReplicaApply::SnapshotBegin => {
                self.replica_snapshot_buf.clear();
            }
            ReplicaApply::SnapshotChunk(bytes) => {
                self.replica_snapshot_buf.extend_from_slice(&bytes);
            }
            ReplicaApply::SnapshotEnd { ack_offset, routed, gate } => {
                self.apply_snapshot_end(ack_offset, routed);
                // Only now — with the swapped-in keyspace being what
                // readers will see — may the completion token fire
                // (it lowers the embedder's `-LOADING` gate). Dropping
                // it any earlier reopens reads on the pre-resync state
                // still sitting in this inbox's queue.
                drop(gate);
            }
            ReplicaApply::Frame { offset, argv } => {
                self.apply_replica_frame(&argv);
                // Applied — the next frame carries offset + 1.
                self.replica_applied_next = offset.saturating_add(1);
            }
        }
    }

    /// Replace the local keyspace with a shipped snapshot and jump the
    /// apply position to its `ack_offset`. Split out of
    /// [`Self::apply_replica_event`] to keep that dispatcher under the
    /// house fn-length rule.
    fn apply_snapshot_end(&mut self, ack_offset: u64, routed: bool) {
        let buf = std::mem::take(&mut self.replica_snapshot_buf);
        // A snapshot ship REPLACES local state, it does not merge into
        // it. The load is per-record upsert, so any local residue not
        // present upstream (a rejoining old primary's forked suffix, a
        // stale pre-resync keyspace) must be dropped first — otherwise
        // the fork survives the "discard".
        self.store.flushall();
        let res = if routed {
            // Single-source mode: the payload is the whole upstream
            // keyspace — keep only this shard's slice.
            let (id, n) = (self.id, self.nshards);
            kevy_persist::load_snapshot_filtered(
                &mut self.store,
                Cursor::new(buf.as_slice()),
                |key| (kevy_hash::key_hash_slot(key) as usize) % n == id,
            )
        } else {
            kevy_persist::load_snapshot_from(&mut self.store, Cursor::new(buf.as_slice()))
        };
        if let Err(e) = res {
            eprintln!("kevy: shard {} replica snapshot load failed: {e}", self.id);
        }
        // A snapshot load covers the stream up to its ack_offset — the
        // apply position jumps there (plain store, not max: a
        // fork-discard resync genuinely rewinds and the truth must
        // show it).
        self.replica_applied_next = ack_offset;
        // A bulk load is not keyspace traffic — drop captured events.
        let _ = self.store.take_notify_events();
        self.rewrite_aof_after_resync();
    }

    /// Re-base the local AOF on the post-resync keyspace. The
    /// flushall + snapshot load above bypass the commit path, so
    /// without this the AOF still holds the PRE-resync history —
    /// frames appended after the resync would replay on top of the
    /// wrong base at the next boot (a keyspace the primary discarded,
    /// served as truth until the link comes back). A synchronous
    /// `rewrite_from` closes the window in one atomic rename; resync
    /// is a rare bulk event, so blocking the reactor for one dump is
    /// the honest trade. Any in-flight background persist job holds a
    /// stale pre-resync view whose commit renames over the live AOF —
    /// drain it FIRST so this rebase is the last writer.
    fn rewrite_aof_after_resync(&mut self) {
        if self.aof.is_none() {
            return;
        }
        self.drain_persist_on_shutdown();
        let Some(aof) = self.aof.as_mut() else { return };
        if let Err(e) = aof.rewrite_from(&self.store) {
            eprintln!(
                "kevy: shard {} post-resync aof rewrite failed: {e} — local log \
                 still holds pre-resync history until the next rewrite",
                self.id,
            );
        }
    }

    /// Dispatch one replicated mutation frame against the local
    /// `Store`. The [`ReplicatedApplyGuard`] suppresses the source
    /// push inside `post_write_housekeeping`; everything else (AOF,
    /// WATCH bump, keyspace notify, BLOCK wake) fires normally.
    fn apply_replica_frame(&mut self, argv: &crate::Argv) {
        let _guard = ReplicatedApplyGuard::enter();
        let resolved = self.commands.resolve(argv);
        let meta = DispatchMeta {
            is_write: resolved.is_write,
            wake_idx: resolved.wake_idx,
            key_idx: match resolved.route {
                crate::Route::Single(idx) => u8::try_from(idx).ok(),
                _ => None,
            },
        };
        self.reply_scratch.clear();
        self.commands
            .dispatch_into(&mut self.store, argv, &mut self.reply_scratch);
        self.reply_scratch.clear();
        self.post_write_housekeeping(argv, meta);
    }
}
