//! Stall diagnostics for the io_uring reactor: the opt-in
//! `KEVY_DEBUG_STALL_MS` dump that names every connection which can no
//! longer make progress on its own. Split out of [`crate::uring_arm`]
//! so that file stays under the 500-LOC house rule.

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::{BigArgState, UringConn};
use kevy_map::KevyMap;

/// Name the big-arg sub-state: which variant, how far the body got, and
/// every flag the cycle waits on. `big_arg=true` alone could not tell a
/// legitimately in-flight read from a wedge — that missing detail cost
/// three rounds of source-reasoning on the deep-pipeline wedge.
fn describe_big_arg(uc: &UringConn) -> String {
    match uc.pending_big_arg.as_deref() {
        None => String::from("none"),
        Some(BigArgState::Frame { frame, total }) => format!("Frame({}/{total})", frame.len()),
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
        Some(BigArgState::BareSetReading { body, body_len, crlf_seen, .. }) => {
            format!("Reading(body {}/{body_len} crlf={crlf_seen})", body.len())
        }
    }
}

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
            // `closing` is NOT a reason to skip, and used to be one.
            //
            // The other two are: an armed recv and an in-flight write each
            // have an outstanding completion that brings the conn back.
            // A closing conn has no such guarantee — it is waiting on
            // `uring_reap_closing`, whose own two terms can stay false
            // forever, and whose candidate list it may never have entered.
            // Skipping it made the dump silent on the one shape it was
            // built to name: decided-to-close, never landed.
            if uc.closing {
                self.dump_closing_conn(*cid, conn, uc);
                continue;
            }
            if uc.recv_armed || uc.write_inflight {
                continue;
            }
            self.dump_stalled_conn(*cid, conn, uc);
        }
    }

    /// One stalled conn's line: every flag its state machine could be
    /// waiting on, so the wedge shape is readable without a debugger.
    fn dump_stalled_conn(&self, cid: u64, conn: &crate::conn::Conn, uc: &UringConn) {
        eprintln!(
            "kevy: STALL shard {} conn {cid}: recv_armed=false arm_queued={} \
             in_arm_pending={} big_arg={} cancel_pending={} read_pending={} \
             rearm_recv={} output={} write_pending={} \
             pending_slots={} next_seq={} next_emit={}",
            self.id,
            uc.arm_queued,
            self.arm_pending.contains(&cid),
            describe_big_arg(uc),
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

    /// One closing conn's line, reported in the reap's own terms.
    ///
    /// [`Self::uring_reap_closing`] drops a conn when it is a candidate
    /// AND `writes_quiet && drained`. Each of those is printed here
    /// separately, alongside candidate-list membership, because a closing
    /// conn that outlives one dump interval failed exactly one of them and
    /// no other reading says which. A conn absent from the candidate list
    /// is never retried at all, so that flag is not a detail: it is the
    /// difference between "waiting on a write" and "unreachable".
    fn dump_closing_conn(&self, cid: u64, conn: &crate::conn::Conn, uc: &UringConn) {
        let writes_quiet = !uc.write_inflight && uc.write_buf.is_empty();
        let drained = conn.output.is_empty() && conn.pending.is_empty() && conn.write_pos == 0;
        eprintln!(
            "kevy: STALL shard {} conn {cid}: CLOSING reap_candidate={} \
             writes_quiet={writes_quiet} (write_inflight={} write_buf={}) \
             drained={drained} (output={} pending={} write_pos={}) \
             recv_armed={} big_arg={} cancel_pending={}",
            self.id,
            self.closing_uring_conns.contains(&cid),
            uc.write_inflight,
            uc.write_buf.len(),
            conn.output.len(),
            conn.pending.len(),
            conn.write_pos,
            uc.recv_armed,
            describe_big_arg(uc),
            uc.big_arg_cancel_pending,
        );
    }
}

/// The dump's first deadline, backdated by one interval so the opening
/// heartbeat lands on the reactor's FIRST tick rather than one interval
/// into its life.
///
/// The reason is not that a passing run should show a heartbeat — it
/// cannot: the test harness captures a spawned thread's output too, and
/// prints it only for a failing test. The reason is that the block it
/// DOES print should open with the dump rather than 250ms into it, so a
/// failure that lands quickly still carries one.
///
/// `checked_sub` may decline this early in a process's life; the first
/// heartbeat is then one interval late, which is the old behaviour and
/// not a wrong one. With no interval set there is nothing to backdate
/// and [`Shard::uring_maybe_dump_stalled`] returns on its first line
/// anyway.
pub(crate) fn stall_dump_start(every: Option<std::time::Duration>) -> std::time::Instant {
    let now = std::time::Instant::now();
    every.and_then(|iv| now.checked_sub(iv)).unwrap_or(now)
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

#[cfg(test)]
mod tests {
    use super::stall_dump_start;
    use std::time::{Duration, Instant};

    /// With the dump off there is no interval to backdate, and the
    /// returned instant must not sit in the past: a stale deadline would
    /// make the disabled path do arithmetic against a fabricated time.
    #[test]
    fn no_interval_starts_the_clock_at_now() {
        let before = Instant::now();
        let started = stall_dump_start(None);
        let after = Instant::now();
        assert!(started >= before && started <= after, "not between the two readings");
    }

    /// The property the reactor depends on: the first tick must already
    /// be past the deadline, so the opening heartbeat lands immediately
    /// rather than one interval into the run. A run shorter than one
    /// interval otherwise prints nothing, and nothing is also what a
    /// dump that was never switched on prints.
    ///
    /// `checked_sub` can decline within one interval of boot, and this
    /// asserts as if it cannot: Linux's `Instant` is CLOCK_MONOTONIC,
    /// so declining requires a machine less than 250 ms old, which is
    /// not a machine that is running tests.
    #[test]
    fn an_interval_backdates_the_deadline_by_at_least_that_interval() {
        let iv = Duration::from_millis(250);
        let elapsed = Instant::now().duration_since(stall_dump_start(Some(iv)));
        assert!(elapsed >= iv, "backdated by {elapsed:?}, wanted at least {iv:?}");
    }

    /// A longer interval backdates further — the amount tracks the
    /// argument rather than being a fixed nudge.
    #[test]
    fn a_longer_interval_backdates_further() {
        let short = stall_dump_start(Some(Duration::from_millis(100)));
        let long = stall_dump_start(Some(Duration::from_secs(5)));
        assert!(long < short, "5s did not backdate further than 100ms");
    }
}
