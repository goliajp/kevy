//! What `submit_and_wait` decides before it makes a syscall, and what it
//! makes of the kernel's drop counter afterwards.
//!
//! Split out of `ring.rs` at the 500-line ceiling, and the seam is real:
//! nothing here touches the ring. That is also what makes it testable —
//! on a healthy, non-SQPOLL ring, `overflowed` and `sqpoll` are false
//! forever, so as methods these decisions were half unexecutable code.

use std::io;

use crate::ffi::IORING_ENTER_GETEVENTS;

/// How many submissions the kernel refused since `last`.
///
/// A free function so both answers are reachable from a test: on a
/// healthy ring the counter never moves, so the reporting branch would
/// be code no coverage run could execute — the same shape that put
/// `page_size_matches` on the ratchet.
///
/// `wrapping_sub` because the kernel's counter is a `u32` that only ever
/// grows. A ring that dropped four billion submissions has bigger
/// problems than this arithmetic, but reporting a nonsense count is
/// still worse than reporting a small one.
pub(crate) fn dropped_since(last: u32, now: u32) -> u32 {
    now.wrapping_sub(last)
}

/// Whether an enter may be skipped this iteration.
///
/// Pure, and separate from the ring, because on a healthy ring
/// `overflowed` is always false and on this engine `sqpoll` is always
/// false — so half of this decision was code no coverage run could
/// execute while it lived inside the method.
pub(crate) fn may_skip_enter(overflowed: bool, to_submit: u32, wait_nr: u32, sqpoll: bool) -> bool {
    // An overflow must reach the kernel to be drained, so it defeats the
    // skip no matter how quiet the ring looks.
    !overflowed && to_submit == 0 && wait_nr == 0 && !sqpoll
}

/// Empty iterations between forced enters when the kernel reports
/// pending completion work through `IORING_SQ_TASKRUN`: the flag says
/// when an enter is needed, so this only bounds the cost of a flag the
/// kernel failed to raise.
pub(crate) const ENTER_SAFETY_ITERS: u32 = 512;

/// Whether an empty iteration (nothing to submit, nothing to wait for)
/// still has to enter: `iters` empty iterations have passed since the last
/// enter. With the task-run flag the kernel says when; without it, every
/// `skip_threshold`-th iteration enters so deferred completions run.
pub(crate) fn empty_iteration_enters(
    iters: u32,
    taskrun_flag: Option<bool>,
    skip_threshold: u32,
) -> bool {
    match taskrun_flag {
        Some(pending) => pending || iters >= ENTER_SAFETY_ITERS,
        None => iters >= skip_threshold,
    }
}

/// The `io_uring_enter` flags for this call.
///
/// `GETEVENTS` when the caller is waiting for a completion, and also
/// when the kernel has parked completions on its overflow list — that
/// list is flushed on an enter which asks for events and on nothing
/// else.
pub(crate) fn enter_flags_for(wait_nr: u32, overflowed: bool) -> u32 {
    if wait_nr > 0 || overflowed { IORING_ENTER_GETEVENTS } else { 0 }
}

/// A dropped-submission count as a result.
///
/// Separate from the read so both answers are reachable: on a healthy
/// ring the counter never moves, and the reporting arm would otherwise
/// be unexecutable code.
pub(crate) fn dropped_error(lost: u32) -> io::Result<()> {
    if lost == 0 {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "io_uring dropped {lost} submission(s); they will never complete"
    )))
}

#[cfg(test)]
mod enter_policy_tests {
    use super::{
        ENTER_SAFETY_ITERS, dropped_error, empty_iteration_enters, enter_flags_for, may_skip_enter,
    };
    use crate::ffi::IORING_ENTER_GETEVENTS;

    /// The quiet iteration this engine spends most of its life in, and
    /// every reason it stops being quiet.
    #[test]
    fn an_enter_is_skipped_only_when_there_is_nothing_the_kernel_must_hear() {
        assert!(may_skip_enter(false, 0, 0, false), "idle non-SQPOLL ring");
        assert!(!may_skip_enter(true, 0, 0, false), "an overflow must be drained");
        assert!(!may_skip_enter(false, 1, 0, false), "a submission must be delivered");
        assert!(!may_skip_enter(false, 0, 1, false), "a waiter must be served");
        assert!(!may_skip_enter(false, 0, 0, true), "SQPOLL has its own skip");
    }

    /// With the task-run flag the kernel decides; the iteration bound only
    /// catches a flag that never came. Without it, every N-th iteration.
    #[test]
    fn an_empty_iteration_enters_when_work_waits_or_a_bound_is_reached() {
        assert!(!empty_iteration_enters(1, Some(false), 2));
        assert!(!empty_iteration_enters(ENTER_SAFETY_ITERS - 1, Some(false), 2));
        assert!(empty_iteration_enters(1, Some(true), 2), "flagged work runs at once");
        assert!(empty_iteration_enters(ENTER_SAFETY_ITERS, Some(false), 2), "the safety bound");
        assert!(!empty_iteration_enters(1, None, 2));
        assert!(empty_iteration_enters(2, None, 2), "no flag: every second iteration");
    }

    /// The overflow list is flushed only on an enter that asks for
    /// events, which is why this is not just `wait_nr > 0`.
    #[test]
    fn getevents_is_asked_for_when_waiting_or_when_the_kernel_parked_completions() {
        assert_eq!(enter_flags_for(0, false), 0);
        assert_eq!(enter_flags_for(1, false), IORING_ENTER_GETEVENTS);
        assert_eq!(enter_flags_for(0, true), IORING_ENTER_GETEVENTS, "overflow needs a drain");
        assert_eq!(enter_flags_for(3, true), IORING_ENTER_GETEVENTS);
    }

    #[test]
    fn a_still_drop_counter_is_ok_and_any_movement_is_an_error() {
        assert!(dropped_error(0).is_ok());
        let e = dropped_error(2).expect_err("a dropped submission must be reported");
        assert!(e.to_string().contains('2'), "the error must say how many: {e}");
        assert!(e.to_string().contains("never complete"), "and why it matters: {e}");
    }
}

#[cfg(test)]
mod dropped_tests {
    use super::dropped_since;

    /// Both answers, including the one a healthy ring never gives.
    #[test]
    fn a_still_counter_reports_nothing_and_a_moved_one_reports_the_delta() {
        assert_eq!(dropped_since(0, 0), 0, "an untouched ring must report nothing");
        assert_eq!(dropped_since(7, 7), 0);
        assert_eq!(dropped_since(0, 1), 1);
        assert_eq!(dropped_since(5, 9), 4, "the delta, not the total");
        // The kernel's counter is u32 and only grows; a wrap must not
        // report four billion.
        assert_eq!(dropped_since(u32::MAX, 0), 1);
        assert_eq!(dropped_since(u32::MAX - 1, 1), 3);
    }
}
