//! Replica-side apply path (T1.29(c) + (b)) — the [`Shard`] half of
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
        // Take ownership of all currently-queued events without
        // blocking. `try_iter` yields until the channel is empty;
        // we cap the per-tick budget to keep the reactor responsive
        // when a flood of frames lands at once.
        const MAX_PER_TICK: usize = 1024;
        let mut events = Vec::with_capacity(64);
        for ev in inbox.inner.try_iter().take(MAX_PER_TICK) {
            events.push(ev);
        }
        for ev in events {
            self.apply_replica_event(ev);
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
            ReplicaApply::SnapshotEnd { ack_offset: _ } => {
                let buf = std::mem::take(&mut self.replica_snapshot_buf);
                if let Err(e) = kevy_persist::load_snapshot_from(
                    &mut self.store,
                    Cursor::new(buf.as_slice()),
                ) {
                    eprintln!(
                        "kevy: shard {} replica snapshot load failed: {e}",
                        self.id,
                    );
                }
            }
            ReplicaApply::Frame { offset: _, argv } => {
                self.apply_replica_frame(&argv);
            }
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
