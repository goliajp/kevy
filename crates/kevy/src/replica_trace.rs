//! Trace-only observability helpers for the replica runner — the
//! availgate failover-wedge instrumented-reproduction probe surface
//! (replica side). Split from [`crate::replica_runner`] to keep that
//! file under the 500-LOC house rule. Every fn gates on
//! [`kevy_rt::repl_trace`] and costs nothing when
//! `KEVY_DEBUG_REPL_TRACE` is off.

use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};

/// The handshake result as this runner sees it — what the primary
/// acked vs the local data claim, i.e. whether the silent `from 0`
/// generation adoption is about to fire.
pub(crate) fn trace_session_start(runner_slot: usize, client: &ReplicaClient, data_gen: u64) {
    if !kevy_rt::repl_trace() {
        return;
    }
    kevy_rt::repl_trace_line(format_args!(
        "runner slot {runner_slot}: session up — primary acked \
         gen {} from {}, local data_gen {data_gen}",
        client.primary_gen_at_handshake(),
        client.expected_offset(),
    ));
}

/// The session's history-shaping wire events (snapshot window + the
/// first live frame after each (re)connect).
pub(crate) fn trace_session_event(
    runner_slot: usize,
    event: &ReplicaEvent,
    traced_first_frame: &mut bool,
) {
    if !kevy_rt::repl_trace() {
        return;
    }
    match event {
        ReplicaEvent::SnapshotBegin => {
            kevy_rt::repl_trace_line(format_args!("runner slot {runner_slot}: snapshot begin"));
        }
        ReplicaEvent::SnapshotEnd { ack_offset } => {
            kevy_rt::repl_trace_line(format_args!(
                "runner slot {runner_slot}: snapshot end, resume from {ack_offset}",
            ));
        }
        ReplicaEvent::Frame(frame) if !*traced_first_frame => {
            *traced_first_frame = true;
            kevy_rt::repl_trace_line(format_args!(
                "runner slot {runner_slot}: first frame offset {}",
                frame.offset,
            ));
        }
        _ => {}
    }
}
