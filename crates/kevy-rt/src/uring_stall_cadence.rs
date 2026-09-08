//! The stall dump's cadence: the `KEVY_DEBUG_STALL_MS` interval and the
//! first deadline derived from it.
//!
//! Split from [`crate::uring_stalldump`] because it is pure and that
//! module is not: the dump needs a `Shard`, a `Conn` and a `UringConn`,
//! all of which exist only on Linux, so a test living beside it can run
//! on one platform. This half is arithmetic over `std::time`, and the
//! `test` in the cfg — the same one [`crate::uring_write_linearize`]
//! carries — is what lets it be checked everywhere.

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
/// and [`crate::shard::Shard::uring_maybe_dump_stalled`] returns on its first line
/// anyway.
pub(crate) fn stall_dump_start(every: Option<std::time::Duration>) -> std::time::Instant {
    stall_dump_start_from(std::time::Instant::now(), every)
}

/// [`stall_dump_start`] with the clock passed in, so the arithmetic can
/// be asserted exactly.
///
/// Reading the clock twice cannot express this property: two
/// `Instant::now()` calls are not guaranteed to be ordered, and the
/// first version of this test — which measured the gap between the
/// function's own reading and a later one — failed CI by **40 ns**
/// against a 250 ms interval. That test was measuring the host's clock,
/// not this function, and a check that fails on its own noise teaches
/// people to re-run until green.
fn stall_dump_start_from(
    now: std::time::Instant,
    every: Option<std::time::Duration>,
) -> std::time::Instant {
    every.and_then(|iv| now.checked_sub(iv)).unwrap_or(now)
}

/// Parse the `KEVY_DEBUG_STALL_MS` value into a cadence.
///
/// `None` — the variable absent, unparseable, or zero — disables the
/// dump entirely. Zero is folded into "off" rather than "every tick"
/// because a dump on every tick is not a diagnostic, it is a firehose
/// that changes the timing it is trying to observe.
pub(crate) fn parse_stall_dump_interval(raw: Option<&str>) -> Option<std::time::Duration> {
    raw.and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(std::time::Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::{parse_stall_dump_interval, stall_dump_start, stall_dump_start_from};
    use std::time::{Duration, Instant};

    /// With the dump off there is no interval to backdate, and the
    /// returned instant must be the reading itself: a stale deadline
    /// would make the disabled path do arithmetic against a fabricated
    /// time. This one goes through the public wrapper, so the wrapper is
    /// executed rather than merely compiled.
    #[test]
    fn no_interval_starts_the_clock_at_now() {
        let now = Instant::now();
        assert_eq!(stall_dump_start_from(now, None), now);

        let before = Instant::now();
        let started = stall_dump_start(None);
        let after = Instant::now();
        assert!(started >= before && started <= after, "not between the two readings");
    }

    /// The property the reactor depends on: the first tick is already
    /// past the deadline, so the opening heartbeat lands immediately
    /// rather than one interval into the run. Exact, because the clock
    /// is the argument — see [`super::stall_dump_start_from`].
    #[test]
    fn an_interval_backdates_the_deadline_by_exactly_that_interval() {
        let now = Instant::now();
        for ms in [1u64, 250, 5_000] {
            let iv = Duration::from_millis(ms);
            let started = stall_dump_start_from(now, Some(iv));
            assert_eq!(now.duration_since(started), iv, "interval {ms}ms");
        }
    }

    #[test]
    fn an_absent_unparseable_or_zero_value_disables_the_dump() {
        for raw in [None, Some(""), Some("0"), Some("abc"), Some("-1"), Some("12ms")] {
            assert_eq!(parse_stall_dump_interval(raw), None, "raw {raw:?}");
        }
    }

    #[test]
    fn a_positive_value_is_read_as_milliseconds() {
        assert_eq!(parse_stall_dump_interval(Some("250")), Some(Duration::from_millis(250)));
        assert_eq!(parse_stall_dump_interval(Some("1")), Some(Duration::from_millis(1)));
    }

    /// A longer interval backdates further — the amount tracks the
    /// argument rather than being a fixed nudge.
    #[test]
    fn a_longer_interval_backdates_further() {
        let now = Instant::now();
        let short = stall_dump_start_from(now, Some(Duration::from_millis(100)));
        let long = stall_dump_start_from(now, Some(Duration::from_secs(5)));
        assert!(long < short, "5s did not backdate further than 100ms");
    }
}
