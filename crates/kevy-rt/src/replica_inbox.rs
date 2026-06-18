//! Cross-thread inbox from an external replica runner into a
//! [`Shard`]'s reactor thread. The replica runner lives on its own OS
//! thread (it does blocking `TcpStream` reads from the upstream
//! primary via `ReplicaClient`); applying mutations to the shard's
//! `Store` must happen on the shard's reactor thread, so the runner
//! drops events into this channel and the shard drains it once per
//! tick.
//!
//! The kevy server (the embedder) creates one [`ReplicaInbox`] pair
//! per shard before `Runtime::run`, hands the receivers to the
//! runtime via `with_replica_inboxes`, and keeps the senders to wire
//! into the runner threads. v1.18 spawns one runner per shard
//! (matching the primary's per-shard listener layout), so the
//! channels are 1:1.
//!
//! v1.18 cap: events are unbounded. Each [`ReplicaApply::Frame`]
//! carries an owned [`Argv`] (snapshot path is `Vec<u8>` chunks); for
//! a slow shard this can grow. Backpressure / capping is tracked as a
//! follow-up — the v1.18 model assumes the shard's apply rate matches
//! the upstream emit rate (single-machine cluster). The unbounded
//! channel never blocks the runner thread, so a stuck shard never
//! stalls the runner's TCP read (it just buffers).

use std::sync::mpsc::{Receiver, SendError, Sender, channel};

use crate::Argv;

/// One event delivered from a replica runner to its target shard.
/// Mirrors `kevy_replicate::replica::ReplicaEvent` except `Frame`
/// carries an owned [`Argv`] (already decoded by the runner) instead
/// of a `DecodedFrame { offset, argv }` — the offset is gap-checked
/// by the runner on the way in, so the shard doesn't need it.
#[derive(Debug)]
pub enum ReplicaApply {
    /// Upstream started shipping a full snapshot. The shard should
    /// reset its accumulating snapshot buffer.
    SnapshotBegin,
    /// One chunk of snapshot bytes. The shard appends to its buffer.
    SnapshotChunk(Vec<u8>),
    /// Upstream finished the snapshot. The shard hands its buffered
    /// bytes to `kevy_persist::load_snapshot_from` (replacing the
    /// `Store` contents) and resumes at `ack_offset` for live frames.
    SnapshotEnd { ack_offset: u64 },
    /// One live mutation frame to be applied via `kevy::dispatch`
    /// (inside a [`crate::ReplicatedApplyGuard`] scope so the apply
    /// doesn't re-push into this shard's downstream
    /// `ReplicationSource`).
    Frame { offset: u64, argv: Argv },
}

/// Sender end of a per-shard replica inbox. `Send + Clone + Sync`
/// (one std::sync::mpsc::Sender, no extra state) so the embedder can
/// hand it freely to runner threads.
#[derive(Clone)]
pub struct ReplicaInboxSender {
    inner: Sender<ReplicaApply>,
}

impl ReplicaInboxSender {
    /// Send one event to the target shard. Fails only when the shard
    /// has dropped its receiver (the runtime stopped or the shard
    /// crashed) — the runner should treat that as "no more apply
    /// possible" and exit.
    pub fn send(&self, ev: ReplicaApply) -> Result<(), SendError<ReplicaApply>> {
        self.inner.send(ev)
    }
}

/// Receiver end. Lives inside the (private) `Shard`; drained once
/// per reactor tick. Constructed by [`replica_inbox_pair`] and
/// handed to the runtime via `Runtime::with_replica_inboxes`.
pub struct ReplicaInboxReceiver {
    pub(crate) inner: Receiver<ReplicaApply>,
}

/// Create a matched (sender, receiver) pair for one shard's replica
/// inbox. The embedder calls this `nshards` times before
/// `Runtime::run`.
#[must_use]
pub fn replica_inbox_pair() -> (ReplicaInboxSender, ReplicaInboxReceiver) {
    let (tx, rx) = channel();
    (
        ReplicaInboxSender { inner: tx },
        ReplicaInboxReceiver { inner: rx },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_round_trips_one_event() {
        let (tx, rx) = replica_inbox_pair();
        tx.send(ReplicaApply::SnapshotBegin).unwrap();
        match rx.inner.recv().unwrap() {
            ReplicaApply::SnapshotBegin => {}
            other => panic!("expected SnapshotBegin, got {other:?}"),
        }
    }

    #[test]
    fn drop_receiver_makes_send_fail() {
        let (tx, rx) = replica_inbox_pair();
        drop(rx);
        let err = tx.send(ReplicaApply::SnapshotBegin).unwrap_err();
        match err.0 {
            ReplicaApply::SnapshotBegin => {}
            other => panic!("expected payload roundtrip, got {other:?}"),
        }
    }

    #[test]
    fn sender_is_clone_send_sync() {
        fn assert_traits<T: Clone + Send + Sync>() {}
        assert_traits::<ReplicaInboxSender>();
    }
}
