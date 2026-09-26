//! What the io_uring reactor does once its busy-poll has run dry: keep
//! polling for a short hot window after forwarded work, or park. Pure, so
//! it is tested on every platform; the reasoning is in the idle-ladder
//! comment in `run_uring`.

use std::time::{Duration, Instant};

/// How long an owner shard keeps polling, unparked, after a batch of
/// forwarded work.
pub(crate) const HOT_IDLE: Duration = Duration::from_micros(200);
/// The smallest inbound drain that opens a hot window. Sequential -c1
/// traffic drains one message per request and parks straight away.
pub(crate) const HOT_IDLE_BATCH_MIN: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum IdleStep {
    /// Keep polling: forwards and socket input are picked up on the next
    /// iteration.
    Poll,
    /// Block in the ring until something arrives.
    Park,
}

/// Decide the next step of an idle episode. `hot_until` is the episode's
/// window, opened here on the first call after a big enough drain; the
/// caller clears it when it parks or when forwarded work arrives.
pub(crate) fn idle_step(
    last_inbound_batch: usize,
    hot_until: &mut Option<Instant>,
    now: Instant,
) -> IdleStep {
    if hot_until.is_none() && last_inbound_batch >= HOT_IDLE_BATCH_MIN {
        *hot_until = Some(now + HOT_IDLE);
    }
    match *hot_until {
        Some(end) if now < end => IdleStep::Poll,
        _ => IdleStep::Park,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_drains_park_at_once() {
        let mut w = None;
        let now = Instant::now();
        assert_eq!(idle_step(0, &mut w, now), IdleStep::Park);
        assert_eq!(idle_step(HOT_IDLE_BATCH_MIN - 1, &mut w, now), IdleStep::Park);
        assert_eq!(w, None);
    }

    #[test]
    fn a_big_drain_polls_for_the_window_then_parks() {
        let mut w = None;
        let t0 = Instant::now();
        assert_eq!(idle_step(32, &mut w, t0), IdleStep::Poll);
        assert_eq!(w, Some(t0 + HOT_IDLE));
        // later calls in the same episode do not push the end back
        let mid = t0 + HOT_IDLE / 2;
        assert_eq!(idle_step(32, &mut w, mid), IdleStep::Poll);
        assert_eq!(w, Some(t0 + HOT_IDLE));
        assert_eq!(idle_step(32, &mut w, t0 + HOT_IDLE), IdleStep::Park);
        assert_eq!(idle_step(32, &mut w, t0 + HOT_IDLE * 3), IdleStep::Park);
    }
}
