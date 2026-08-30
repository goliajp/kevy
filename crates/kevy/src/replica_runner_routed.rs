//! Single-source (embedded-as-primary) half of the replica runner:
//! ONE upstream connection fans events into EVERY shard's inbox.
//! Split from `replica_runner.rs` so each file stays under the
//! 500-LOC house rule.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};
use kevy_rt::{ReplicaApply, ReplicaInboxSender, SnapshotGate};

use crate::replica_runner_events::LoadingGuard;
use crate::state::ReplicaProgress;

/// Route one event fan into N shard inboxes: snapshot control/chunks
/// BROADCAST (each shard loads its own hash slice — SnapshotEnd
/// carries `routed: true`); keyed frames route by hash slot; the
/// keyless flushes broadcast; other keyless frames go to shard 0
/// (pub/sub convention).
pub(crate) fn route_event(
    event: ReplicaEvent,
    from_offset: &mut u64,
    senders: &[ReplicaInboxSender],
    gate: Option<SnapshotGate>,
) -> Result<(), ()> {
    let n = senders.len();
    let send_all = |apply: &dyn Fn() -> ReplicaApply| -> Result<(), ()> {
        for s in senders {
            s.send(apply()).map_err(|_| ())?;
        }
        Ok(())
    };
    match event {
        // Consumed by drain_client_routed; by-argument unreachable.
        ReplicaEvent::Ping { .. } => Ok(()),
        ReplicaEvent::SnapshotBegin => send_all(&|| ReplicaApply::SnapshotBegin),
        ReplicaEvent::SnapshotChunk(bytes) => {
            send_all(&|| ReplicaApply::SnapshotChunk(bytes.clone()))
        }
        ReplicaEvent::SnapshotEnd { ack_offset } => {
            *from_offset = ack_offset;
            // Every shard gets a CLONE of the gate: the loading
            // lowering fires when the last shard's load lands.
            send_all(&|| ReplicaApply::SnapshotEnd { ack_offset, routed: true, gate: gate.clone() })
        }
        ReplicaEvent::Frame(frame) => {
            *from_offset = frame.offset.saturating_add(1);
            let verb = frame.argv.get(0).unwrap_or_default();
            if verb.eq_ignore_ascii_case(b"FLUSHALL") || verb.eq_ignore_ascii_case(b"FLUSHDB") {
                return send_all(&|| ReplicaApply::Frame {
                    offset: frame.offset,
                    argv: frame.argv.clone(),
                });
            }
            let slot = match frame.argv.get(1) {
                Some(key) => (kevy_hash::key_hash_slot(key) as usize) % n,
                None => 0,
            };
            senders[slot]
                .send(ReplicaApply::Frame { offset: frame.offset, argv: frame.argv })
                .map_err(|_| ())
        }
    }
}

/// Drain loop for single-source mode (one upstream conn, all
/// shard inboxes). Same `data_gen` contract as [`drain_client`].
pub(crate) fn drain_client_routed(
    client: &mut ReplicaClient,
    senders: &[ReplicaInboxSender],
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
                if !crate::replica_runner_events::gen_still_matches(generation, ack_gen) {
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
                let gate = loading.observe(&event);
                if route_event(event, &mut from_offset, senders, gate).is_err() {
                    return from_offset;
                }
                crate::replica_runner_events::maybe_ack(
                    client,
                    progress,
                    runner_slot,
                    from_offset,
                    &mut last_ack,
                );
            }
            Some(Err(e)) => {
                eprintln!("kevy: replica runner upstream error: {e}");
                return from_offset;
            }
            None => return from_offset,
        }
    }
    from_offset
}
