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
//! into the runner threads. One runner is spawned per shard
//! (matching the primary's per-shard listener layout), so the
//! channels are 1:1.
//!
//! Known cap: events are unbounded. Each [`ReplicaApply::Frame`]
//! carries an owned [`Argv`] (snapshot path is `Vec<u8>` chunks); for
//! a slow shard this can grow. Backpressure / capping is tracked as a
//! follow-up. The unbounded channel never blocks the runner thread, so
//! a stuck shard never stalls the runner's TCP read (it just buffers).
//!
//! Wake contract: a send signals the shard's [`Waker`] so a reactor
//! parked in `Poller::wait` drains promptly — the flag in
//! [`InboxSignal`] collapses a burst into one self-pipe write. Without
//! this, drain only ran when *other* traffic happened to wake the
//! reactor, and a fast upstream buried a quiet replica in backlog
//! (found by repligate: ~7s of undrained frames after the primary
//! froze, unmasked when `frames_from` went O(B) → O(log B) and the
//! primary started feeding at full speed).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SendError, Sender, channel};
use std::sync::{Arc, OnceLock};

use kevy_sys::Waker;

use crate::Argv;

/// The cross-thread wake bridge shared by a sender/receiver pair. The
/// shard installs its own waker at reactor start; `wake_pending`
/// throttles the self-pipe to one write per drain cycle no matter how
/// many frames a burst carries.
///
/// Aligned to a cache line: eight shards' signals are allocated
/// back-to-back at startup, and an unaligned flag shares its line with
/// a neighbouring shard's — every reactor iteration polls the flag, so
/// a shared line ping-pongs across cores at reactor frequency (the
/// sadd L1-miss A/B that caught it).
#[repr(align(64))]
pub(crate) struct InboxSignal {
    pub(crate) waker: OnceLock<Arc<Waker>>,
    pub(crate) wake_pending: AtomicBool,
}

/// Opaque completion token riding on [`ReplicaApply::SnapshotEnd`].
/// The shard drops it only AFTER the snapshot swap has landed in its
/// `Store`, so the embedder can hang side effects (e.g. lowering a
/// `-LOADING` read gate) on the token's `Drop` and know they fire
/// once the new keyspace — not the one about to be replaced — is
/// what readers will see. Clones share one inner value: in broadcast
/// (single-source) mode every shard holds a clone and the `Drop`
/// fires when the LAST shard finishes its load.
#[derive(Clone)]
pub struct SnapshotGate(#[allow(dead_code)] Arc<dyn std::any::Any + Send + Sync>);

impl SnapshotGate {
    /// Wrap the embedder's drop-hook value.
    #[must_use]
    pub fn new(inner: Arc<dyn std::any::Any + Send + Sync>) -> Self {
        Self(inner)
    }
}

impl std::fmt::Debug for SnapshotGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SnapshotGate")
    }
}

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
    /// `routed = true` (single-source mode) means the payload is
    /// the WHOLE upstream keyspace broadcast to every shard — each
    /// shard loads only its own hash slice.
    /// `gate`: dropped by the shard after the load lands (see
    /// [`SnapshotGate`]); `None` when the runner has nothing to hang
    /// on the completion.
    SnapshotEnd {
        /// Upstream offset the snapshot corresponds to; the shard acks from
        /// here once the load lands.
        ack_offset: u64,
        /// `true` when each shard received only its own hash slice;
        /// `false` when the whole keyspace was broadcast and each shard
        /// must filter.
        routed: bool,
        /// Dropped by the shard once the load completes, which is how the
        /// runner learns it finished. `None` when nothing is waiting.
        gate: Option<SnapshotGate>,
    },
    /// One live mutation frame to be applied via `kevy::dispatch`
    /// (inside a [`crate::ReplicatedApplyGuard`] scope so the apply
    /// doesn't re-push into this shard's downstream
    /// `ReplicationSource`).
    Frame {
        /// Upstream offset this frame sits at, used for the apply position
        /// and the ack.
        offset: u64,
        /// The command to apply, already parsed.
        argv: Argv,
    },
}

/// Sender end of a per-shard replica inbox. `Send + Clone + Sync`
/// (one std::sync::mpsc::Sender, no extra state) so the embedder can
/// hand it freely to runner threads.
#[derive(Clone)]
pub struct ReplicaInboxSender {
    inner: Sender<ReplicaApply>,
    signal: Arc<InboxSignal>,
}

impl ReplicaInboxSender {
    /// Send one event to the target shard, waking its reactor if it
    /// may be parked. Fails only when the shard has dropped its
    /// receiver (the runtime stopped or the shard crashed) — the
    /// runner should treat that as "no more apply possible" and exit.
    pub fn send(&self, ev: ReplicaApply) -> Result<(), SendError<ReplicaApply>> {
        self.inner.send(ev)?;
        // One self-pipe write per drain cycle, not per frame: the flag
        // stays raised until the shard's drain lowers it.
        if !self.signal.wake_pending.swap(true, Ordering::AcqRel)
            && let Some(w) = self.signal.waker.get()
        {
            let _ = w.wake();
        }
        Ok(())
    }
}

/// Receiver end. Lives inside the (private) `Shard`; drained every
/// reactor iteration. Constructed by [`replica_inbox_pair`] and
/// handed to the runtime via `Runtime::with_replica_inboxes`.
pub struct ReplicaInboxReceiver {
    pub(crate) inner: Receiver<ReplicaApply>,
    pub(crate) signal: Arc<InboxSignal>,
}

impl ReplicaInboxReceiver {
    /// Install the owning shard's waker — called once at reactor
    /// start, after which sends interrupt a parked `Poller::wait`.
    pub(crate) fn attach_waker(&self, waker: Arc<Waker>) {
        let _ = self.signal.waker.set(waker);
    }
}

/// Create a matched (sender, receiver) pair for one shard's replica
/// inbox. The embedder calls this `nshards` times before
/// `Runtime::run`.
#[must_use]
pub fn replica_inbox_pair() -> (ReplicaInboxSender, ReplicaInboxReceiver) {
    let (tx, rx) = channel();
    let signal = Arc::new(InboxSignal {
        waker: OnceLock::new(),
        wake_pending: AtomicBool::new(false),
    });
    (
        ReplicaInboxSender { inner: tx, signal: Arc::clone(&signal) },
        ReplicaInboxReceiver { inner: rx, signal },
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
