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

/// The shortest spin before a shard waiting on forwarded replies parks.
pub(crate) const REPLY_SPIN_MIN: Duration = Duration::from_micros(2);
/// The longest.
pub(crate) const REPLY_SPIN_MAX: Duration = Duration::from_micros(50);
/// Replies that take longer than this on average are cheaper to wait for
/// asleep: a nap costs this shard one system call, a spin costs its core
/// for the whole wait.
pub(crate) const REPLY_PARK_BREAK_EVEN: Duration = Duration::from_micros(10);

/// A shard with forwarded commands out and nothing else to do: spin while
/// the replies are expected soon, then nap for about as long as they
/// usually take. The nap does not announce the shard parked, so the owner
/// never pays to wake it; socket input still ends it early. The
/// expectation is a moving average of how long replies took, naps
/// included.
pub(crate) struct ReplyWait {
    since: Option<Instant>,
    iters: u32,
    typical: Duration,
}

impl Default for ReplyWait {
    fn default() -> ReplyWait {
        ReplyWait { since: None, iters: 0, typical: Duration::from_micros(5) }
    }
}

impl ReplyWait {
    /// One idle iteration with replies outstanding; `now` is read on the
    /// first and then on every 32nd.
    pub(crate) fn idle(&mut self, now: impl FnOnce() -> Instant) -> IdleStep {
        let Some(since) = self.since else {
            self.since = Some(now());
            self.iters = 0;
            return IdleStep::Poll;
        };
        self.iters = self.iters.wrapping_add(1);
        if !self.iters.is_multiple_of(32) || now().duration_since(since) < self.window() {
            return IdleStep::Poll;
        }
        IdleStep::Park
    }

    /// Something arrived; if this shard was waiting, it ends the wait.
    pub(crate) fn arrived(&mut self, now: impl FnOnce() -> Instant) {
        if let Some(since) = self.since.take() {
            self.typical = (self.typical * 3 + now().duration_since(since)) / 4;
        }
    }

    /// Other work interrupted the wait: what follows is not a reply's
    /// latency, so it is not measured.
    pub(crate) fn interrupted(&mut self) {
        self.since = None;
    }

    /// How long to nap once the spin is over.
    pub(crate) fn nap(&self) -> Duration {
        self.typical.clamp(REPLY_SPIN_MIN, REPLY_SPIN_MAX)
    }

    /// How long to spin before napping.
    pub(crate) fn window(&self) -> Duration {
        if self.typical > REPLY_PARK_BREAK_EVEN {
            REPLY_SPIN_MIN
        } else {
            (self.typical * 2).clamp(REPLY_SPIN_MIN, REPLY_SPIN_MAX)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `n` idle iterations at `at`, returning the first non-poll step.
    fn idle_for(w: &mut ReplyWait, n: u32, at: Instant) -> IdleStep {
        for _ in 0..n {
            if w.idle(|| at) == IdleStep::Park {
                return IdleStep::Park;
            }
        }
        IdleStep::Poll
    }

    #[test]
    fn fast_replies_are_spun_for_and_never_parked_on() {
        let mut w = ReplyWait::default();
        let t0 = Instant::now();
        for i in 0..20u32 {
            let start = t0 + Duration::from_micros(u64::from(i) * 100);
            assert_eq!(idle_for(&mut w, 64, start), IdleStep::Poll);
            w.arrived(|| start + Duration::from_micros(3));
        }
        let window = w.window().as_nanos();
        assert!((6_000..6_100).contains(&window), "twice the typical 3 µs: {window} ns");
        let start = t0 + Duration::from_millis(10);
        assert_eq!(idle_for(&mut w, 1, start), IdleStep::Poll);
        assert_eq!(idle_for(&mut w, 64, start + Duration::from_micros(5)), IdleStep::Poll);
        assert_eq!(idle_for(&mut w, 64, start + Duration::from_micros(7)), IdleStep::Park);
    }

    #[test]
    fn slow_replies_park_after_the_shortest_spin() {
        let mut w = ReplyWait::default();
        let t0 = Instant::now();
        for i in 0..20u32 {
            let start = t0 + Duration::from_millis(u64::from(i));
            w.idle(|| start);
            w.arrived(|| start + Duration::from_micros(40));
        }
        assert_eq!(w.window(), REPLY_SPIN_MIN);
        let start = t0 + Duration::from_millis(50);
        assert_eq!(idle_for(&mut w, 1, start), IdleStep::Poll);
        assert_eq!(idle_for(&mut w, 64, start + REPLY_SPIN_MIN), IdleStep::Park);
        assert_eq!(w.nap().as_micros(), 39, "about the typical 40 µs, which it approaches");
    }

    #[test]
    fn a_wait_cut_short_by_other_work_teaches_nothing() {
        let mut w = ReplyWait::default();
        let before = w.window();
        let t0 = Instant::now();
        w.idle(|| t0);
        w.interrupted();
        w.arrived(|| t0 + Duration::from_millis(5));
        assert_eq!(w.window(), before, "a long busy stretch is not a slow reply");
    }

    #[test]
    fn the_clock_is_read_at_the_start_and_every_32nd_iteration() {
        let mut w = ReplyWait::default();
        let reads = std::cell::Cell::new(0);
        let t0 = Instant::now();
        for _ in 0..=64 {
            w.idle(|| {
                reads.set(reads.get() + 1);
                t0
            });
        }
        assert_eq!(reads.get(), 3);
    }

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
