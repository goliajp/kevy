//! What a replica runner does with each event, and the loading gate that
//! rides along with it.
//!
//! Split from `replica_runner.rs` for the house 500-LOC rule, at the seam
//! the file already had: everything above owns the thread, the socket and
//! the reconnect loop; everything here decides what one `ReplicaEvent`
//! means — whether it raises or lowers the `-LOADING` gate, what apply it
//! turns into, and which shard inbox it goes to.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};
use kevy_rt::{ReplicaApply, ReplicaInboxSender, SnapshotGate};

use crate::state::ReplicaProgress;

/// Tracks this runner's snapshot-ship window against the shared
/// [`ReplicaProgress`] loading count: raised at `SnapshotBegin`,
/// lowered when the shard-side APPLY of `SnapshotEnd` completes —
/// not when this runner merely reads the event off the wire. The
/// lowering rides as a [`SnapshotGate`] on the apply event; the
/// shard drops it only after the snapshot swap lands, so the
/// `-LOADING` gate never reopens reads on the pre-resync keyspace
/// still queued in the inbox. Early exits from the drain loop (link
/// drop, shard gone, stop) drop the held token instead, so a
/// mid-ship disconnect never strands the replica refusing reads.
pub(crate) struct LoadingToken {
    progress: Arc<ReplicaProgress>,
}

impl Drop for LoadingToken {
    fn drop(&mut self) {
        self.progress.end_loading();
    }
}

pub(crate) struct LoadingGuard {
    progress: Arc<ReplicaProgress>,
    token: Option<Arc<LoadingToken>>,
}

impl LoadingGuard {
    pub(crate) fn new(progress: Arc<ReplicaProgress>) -> Self {
        Self { progress, token: None }
    }

    /// Observe one wire event. For `SnapshotEnd` this hands back the
    /// gate to attach to the apply event(s) — in broadcast mode every
    /// shard gets a clone and the lowering fires when the LAST shard
    /// finishes its load.
    pub(crate) fn observe(&mut self, event: &ReplicaEvent) -> Option<SnapshotGate> {
        match event {
            ReplicaEvent::SnapshotBegin if self.token.is_none() => {
                self.progress.begin_loading();
                self.token = Some(Arc::new(LoadingToken { progress: Arc::clone(&self.progress) }));
                None
            }
            ReplicaEvent::SnapshotEnd { .. } => self.token.take().map(|t| SnapshotGate::new(t)),
            _ => None,
        }
    }
}

/// Drain `next_event` until the peer EOFs / errors. Returns the
/// `from_offset` to resume from on the next reconnect.
///
/// `data_gen` tracking: everything this session delivers belongs to
/// the generation the primary advertised in `+ACK`. The local data
/// ADOPTS it when a whole history lands — at `SnapshotEnd`, or
/// immediately when the session started from offset 0 (nothing local
/// to contradict). A heartbeat carrying a different generation means
/// the primary broke continuity mid-stream (FLUSHALL / promotion) —
/// drop the link; the reconnect handshake lets the fence re-decide.
pub(crate) fn drain_client(
    client: &mut ReplicaClient,
    sender: &ReplicaInboxSender,
    stop: &Arc<AtomicBool>,
    runner_slot: usize,
    progress: &Arc<ReplicaProgress>,
    data_gen: &mut u64,
) -> u64 {
    let mut from_offset = client.expected_offset();
    let ack_gen = client.primary_gen_at_handshake();
    if from_offset == 0 {
        *data_gen = ack_gen;
    }
    let mut last_ack = std::time::Instant::now();
    let mut loading = LoadingGuard::new(Arc::clone(progress));
    let mut traced_first_frame = false;
    while !stop.load(Ordering::Relaxed) {
        match client.next_event() {
            Some(Ok(ReplicaEvent::Ping { generation, primary_offset })) => {
                progress.record_ping(runner_slot, generation, primary_offset, from_offset);
                let _ = client.send_ack(from_offset);
                last_ack = std::time::Instant::now();
                if !gen_still_matches(generation, ack_gen) {
                    return from_offset;
                }
            }
            Some(Ok(event)) => {
                if matches!(event, ReplicaEvent::SnapshotEnd { .. }) {
                    *data_gen = ack_gen;
                }
                crate::replica_trace::trace_session_event(
                    runner_slot,
                    &event,
                    &mut traced_first_frame,
                );
                if forward_event(event, &mut from_offset, &mut loading, sender).is_err() {
                    // Receiver dropped — the shard / runtime is gone;
                    // the runner should also exit.
                    return from_offset;
                }
                maybe_ack(client, progress, runner_slot, from_offset, &mut last_ack);
            }
            Some(Err(e)) => {
                eprintln!("kevy: replica runner upstream error: {e}");
                return from_offset;
            }
            None => return from_offset, // clean peer EOF — reconnect
        }
    }
    from_offset
}

/// Turn one wire event into an apply and hand it to the shard,
/// carrying the loading gate on a `SnapshotEnd` so the `-LOADING`
/// window closes only once the shard has finished loading. `Err` means
/// the receiver is gone.
fn forward_event(
    event: ReplicaEvent,
    from_offset: &mut u64,
    loading: &mut LoadingGuard,
    sender: &ReplicaInboxSender,
) -> Result<(), ()> {
    let gate = loading.observe(&event);
    let mut apply = event_to_apply(event, from_offset);
    if let ReplicaApply::SnapshotEnd { gate: g, .. } = &mut apply {
        *g = gate;
    }
    sender.send(apply).map_err(|_| ())
}

/// The 100 ms ack cadence: report the applied position upstream (and
/// into the election-offset registry) without acking every frame.
pub(crate) fn maybe_ack(
    client: &mut ReplicaClient,
    progress: &Arc<ReplicaProgress>,
    runner_slot: usize,
    from_offset: u64,
    last_ack: &mut std::time::Instant,
) {
    if last_ack.elapsed() >= std::time::Duration::from_millis(100) {
        let _ = client.send_ack(from_offset);
        progress.record_applied(runner_slot, from_offset);
        *last_ack = std::time::Instant::now();
    }
}

/// Heartbeat generation gate: record the primary's position, then
/// judge continuity. `false` = the primary broke continuity mid-stream
/// (FLUSHALL / promotion) — drop the link so the reconnect handshake
/// lets the fence re-decide.
pub(crate) fn gen_still_matches(heartbeat_gen: u64, ack_gen: u64) -> bool {
    if heartbeat_gen == 0 || heartbeat_gen == ack_gen {
        return true;
    }
    eprintln!(
        "kevy: replica runner: primary feed generation moved \
         {ack_gen} -> {heartbeat_gen} mid-stream; re-handshaking"
    );
    false
}

fn event_to_apply(event: ReplicaEvent, from_offset: &mut u64) -> ReplicaApply {
    match event {
        // Pings are consumed by the drain loops before reaching here;
        // BY ARGUMENT unreachable, so fall back to a harmless no-op
        // apply (SnapshotBegin resets nothing on its own).
        ReplicaEvent::Ping { .. } => ReplicaApply::SnapshotBegin,
        ReplicaEvent::SnapshotBegin => ReplicaApply::SnapshotBegin,
        ReplicaEvent::SnapshotChunk(bytes) => ReplicaApply::SnapshotChunk(bytes),
        ReplicaEvent::SnapshotEnd { ack_offset } => {
            *from_offset = ack_offset;
            // The caller attaches the loading gate — this fn is a
            // pure wire→apply shape map.
            ReplicaApply::SnapshotEnd { ack_offset, routed: false, gate: None }
        }
        ReplicaEvent::Frame(frame) => {
            *from_offset = frame.offset.saturating_add(1);
            ReplicaApply::Frame { offset: frame.offset, argv: frame.argv }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_lowers_only_when_the_apply_gate_drops() {
        let progress = Arc::new(ReplicaProgress::default());
        let mut guard = LoadingGuard::new(Arc::clone(&progress));
        assert!(guard.observe(&ReplicaEvent::SnapshotBegin).is_none());
        assert!(progress.loading(), "SnapshotBegin raises the gate");
        let gate = guard
            .observe(&ReplicaEvent::SnapshotEnd { ack_offset: 9 })
            .expect("SnapshotEnd must hand back the gate");
        // The runner has read SnapshotEnd off the wire, but no shard
        // has applied it yet — reads must stay gated (the pre-resync
        // keyspace is still what the store holds).
        assert!(progress.loading(), "wire-read alone must not lower");
        let second_shard = gate.clone(); // broadcast mode copy
        drop(gate);
        assert!(progress.loading(), "one shard's copy still alive");
        drop(second_shard);
        assert!(!progress.loading(), "last apply lowers the gate");
    }

    #[test]
    fn early_exit_drop_lowers_loading() {
        let progress = Arc::new(ReplicaProgress::default());
        let mut guard = LoadingGuard::new(Arc::clone(&progress));
        let _ = guard.observe(&ReplicaEvent::SnapshotBegin);
        assert!(progress.loading());
        drop(guard); // link drop / stop mid-ship
        assert!(!progress.loading(), "mid-ship exit never strands -LOADING");
    }

    #[test]
    fn event_to_apply_snapshot_begin_passthrough() {
        let mut off = 7;
        let out = event_to_apply(ReplicaEvent::SnapshotBegin, &mut off);
        assert!(matches!(out, ReplicaApply::SnapshotBegin));
        assert_eq!(off, 7, "SnapshotBegin must not touch the offset");
    }

    #[test]
    fn event_to_apply_snapshot_end_advances_offset() {
        let mut off = 0;
        let out = event_to_apply(ReplicaEvent::SnapshotEnd { ack_offset: 42 }, &mut off);
        match out {
            ReplicaApply::SnapshotEnd { ack_offset, .. } => assert_eq!(ack_offset, 42),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(off, 42, "SnapshotEnd must jump from_offset to ack_offset");
    }

    #[test]
    fn event_to_apply_frame_advances_offset_by_one() {
        let mut off = 3;
        let frame =
            kevy_replicate::replica::DecodedFrame { offset: 9, argv: kevy_rt::Argv::default() };
        let out = event_to_apply(ReplicaEvent::Frame(frame), &mut off);
        assert!(matches!(out, ReplicaApply::Frame { offset: 9, .. }));
        assert_eq!(off, 10, "Frame must advance to offset + 1");
    }
}
