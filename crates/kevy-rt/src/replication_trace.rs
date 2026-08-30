//! Trace-only observability helpers for the replication pump — the
//! availgate failover-wedge instrumented-reproduction probe surface.
//! Split from [`crate::replication_pump`] / [`crate::replication_io`]
//! to keep those files under the 500-LOC house rule. Every fn here is
//! called behind [`crate::repl_trace`] and costs nothing when the
//! `KEVY_DEBUG_REPL_TRACE` gate is off.

use crate::Commands;
use crate::replication::ReplicaState;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// A conn sitting in AckSent gets neither frames nor heartbeats
    /// from the pump — if the `+ACK` never fully drains the link
    /// wedges silently. Repurposes the (otherwise unused in this
    /// state) `last_ping` slot as a 1s trace cadence.
    pub(crate) fn trace_acksent_pending(&mut self, idx: usize) {
        let conn = &mut self.replicas[idx];
        let due = conn.last_ping.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1));
        if !due {
            return;
        }
        conn.last_ping = Some(std::time::Instant::now());
        crate::repl_trace_line(format_args!(
            "shard {} fd {} in AckSent: {} B output pending",
            self.id,
            conn.fd,
            conn.output.len() - conn.write_off,
        ));
    }

    /// One-line summary of every attached replica conn — captured at
    /// the promotion-bump moment so the crime scene shows which
    /// cursors existed and in which state when the offset space was
    /// fenced.
    pub(crate) fn replicas_brief(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        for c in &self.replicas {
            let _ = match &c.state {
                ReplicaState::HandshakePending => write!(s, "[fd{} handshake]", c.fd),
                ReplicaState::AckSent { from_offset, generation, .. } => {
                    write!(s, "[fd{} acksent gen {generation} from {from_offset}]", c.fd)
                }
                ReplicaState::Streaming { sent_offset, generation, .. } => {
                    write!(s, "[fd{} streaming gen {generation} sent {sent_offset}]", c.fd)
                }
                ReplicaState::SnapshotShipping { ack_offset, generation, .. } => {
                    write!(s, "[fd{} shipping gen {generation} ack {ack_offset}]", c.fd)
                }
                ReplicaState::Closed { .. } => write!(s, "[fd{} closed]", c.fd),
            };
        }
        if s.is_empty() {
            s.push_str("none");
        }
        s
    }

    /// The accepted handshake's presented claim vs the feed's current
    /// position — the fence input the pump will judge.
    pub(crate) fn trace_handshake(&self, idx: usize) {
        let ReplicaState::AckSent { ref replica_id, from_offset, generation } =
            self.replicas[idx].state
        else {
            return;
        };
        let (feed_gen, feed_next) =
            self.replicate.as_ref().map_or((0, 0), |f| (f.generation(), f.source().next_offset()));
        crate::repl_trace_line(format_args!(
            "shard {} fd {} handshake accepted: replica '{replica_id}' \
             presented gen {generation} from {from_offset} | feed gen {feed_gen} \
             next {feed_next}",
            self.id, self.replicas[idx].fd,
        ));
    }
}
