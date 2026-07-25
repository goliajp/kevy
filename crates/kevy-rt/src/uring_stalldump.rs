//! Stall diagnostics for the io_uring reactor: the opt-in
//! `KEVY_DEBUG_STALL_MS` dump that names every connection which can no
//! longer make progress on its own. Split out of [`crate::uring_arm`]
//! so that file stays under the 500-LOC house rule.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::{BigArgState, UringConn};
use kevy_map::KevyMap;

impl<C: Commands> Shard<C> {
    /// Print every conn that can no longer make progress on its own —
    /// opt-in via `KEVY_DEBUG_STALL_MS=<ms>`, off (one `Option` check on
    /// the tick path) otherwise.
    ///
    /// The predicate is "no recv armed and no reason to be visited
    /// again": such a conn is invisible to [`Self::uring_arm_conns`],
    /// which walks only `arm_pending`, and has no outstanding completion
    /// to bring it back. `arm_queued` is reported alongside actual queue
    /// membership because a conn whose flag says "already queued" while
    /// the queue does not contain it is permanently unreachable —
    /// [`Self::mark_arm_pending`] short-circuits on that flag, so every
    /// later attempt to wake the conn is a no-op.
    ///
    /// Written for `bench/xshardwedge.sh`, which reproduces exactly that
    /// shape. The reactor keeps looping during that wedge (the bounded
    /// park wakes on its timeout, which is why threads read 0% CPU rather
    /// than spinning) and `CLIENT LIST` — an all-shards fan-out — still
    /// answers and still lists the wedged conn, so the shard and its
    /// cross-core messaging are fine and the fault is local to one conn.
    pub(crate) fn uring_maybe_dump_stalled(
        &self,
        every: Option<std::time::Duration>,
        last: &mut std::time::Instant,
        now: std::time::Instant,
        io: &KevyMap<u64, UringConn>,
    ) {
        let Some(iv) = every else { return };
        if now.duration_since(*last) < iv {
            return;
        }
        *last = now;
        // Heartbeat first, unconditionally: without it a silent dump is
        // ambiguous between "ran and found nothing" and "never ran", and
        // the first capture of this wedge hit exactly that ambiguity.
        // The counters are the cross-core ones worth having anyway.
        eprintln!(
            "kevy: STALLDUMP shard {} conns={} arm_pending={} xshard_inflight={} \
             backlog={} dirty={}",
            self.id,
            self.conns.len(),
            self.arm_pending.len(),
            self.xshard_inflight,
            self.backlog.iter().map(std::collections::VecDeque::len).sum::<usize>(),
            self.dirty.len(),
        );
        for (cid, conn) in self.conns.iter() {
            let Some(uc) = io.get(cid) else {
                eprintln!("kevy: STALL shard {} conn {cid}: no UringConn entry", self.id);
                continue;
            };
            if uc.recv_armed || uc.write_inflight || uc.closing {
                continue;
            }
            // Name the big-arg sub-state: which variant, how far the body
            // got, and every flag the cycle waits on. "big_arg=true" alone
            // could not tell a legitimately in-flight read from a wedge.
            let big = match uc.pending_big_arg.as_deref() {
                None => String::from("none"),
                Some(BigArgState::Frame { frame, total }) => {
                    format!("Frame({}/{total})", frame.len())
                }
                Some(BigArgState::BareSetCancelling {
                    body,
                    body_len,
                    crlf_seen,
                    cancel_acked,
                    target_canceled,
                    ..
                }) => format!(
                    "Cancelling(body {}/{body_len} crlf={crlf_seen} \
                     cancel_acked={cancel_acked} target_canceled={target_canceled})",
                    body.len()
                ),
                Some(BigArgState::BareSetReading {
                    body,
                    body_len,
                    crlf_seen,
                    ..
                }) => format!("Reading(body {}/{body_len} crlf={crlf_seen})", body.len()),
            };
            eprintln!(
                "kevy: STALL shard {} conn {cid}: recv_armed=false arm_queued={} \
                 in_arm_pending={} big_arg={big} cancel_pending={} read_pending={} \
                 rearm_recv={} output={} write_pending={} \
                 pending_slots={} next_seq={} next_emit={}",
                self.id,
                uc.arm_queued,
                self.arm_pending.contains(cid),
                uc.big_arg_cancel_pending,
                uc.big_arg_read_pending,
                uc.big_arg_rearm_recv,
                !conn.output.is_empty() || !conn.output_arcs.is_empty(),
                uc.write_off < uc.write_buf.len() || !uc.write_arcs.is_empty(),
                conn.pending.len(),
                conn.next_seq,
                conn.next_emit,
            );
        }
    }
}

/// Stall-dump cadence from `KEVY_DEBUG_STALL_MS`; `None` (the default)
/// disables [`Shard::uring_maybe_dump_stalled`] entirely.
pub(crate) fn stall_dump_interval() -> Option<std::time::Duration> {
    std::env::var("KEVY_DEBUG_STALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(std::time::Duration::from_millis)
}
