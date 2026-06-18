//! io_uring reactor's bounded park — split out of [`crate::uring_reactor`]
//! so that file stays under the project's 500-LOC ceiling. Same
//! `impl Shard`, called from `run_uring`.

#![cfg(target_os = "linux")]

use std::io;
use std::sync::atomic::{Ordering, fence};

use kevy_uring::{IoUring, KernelTimespec};

use crate::Commands;
use crate::shard::Shard;
use crate::uring_conn::ParkState;
use crate::uring_reactor::{OP_TIMEOUT, OP_WAKER};

impl<C: Commands> Shard<C> {
    /// Blocking wait, epoll-park equivalent: publish `parked[me]`, close the
    /// park/wake race with a fenced re-drain (same pairing as `Shard::run` /
    /// `flush_wakes`; loom-verified there), then block in
    /// `submit_and_wait(1)` until any CQE — socket I/O, the waker-pipe read
    /// (a peer pushed to our inbox), or the bounding timeout (tick cadence,
    /// default 50 ms). The CQEs are reaped by the next loop iteration.
    pub(crate) fn uring_park(
        &mut self,
        ring: &mut IoUring,
        park: &mut ParkState,
    ) -> io::Result<()> {
        let me = self.id;
        self.parked[me].store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if self.uring_drain_inbound() {
            // A push landed in the race window — process it, don't block.
            self.parked[me].store(false, Ordering::SeqCst);
            return Ok(());
        }
        if !park.waker_armed {
            // SAFETY: `park` lives on `run_uring`'s stack for the reactor's
            // whole life, so `wake_buf` outlives the SQE.
            park.waker_armed = unsafe {
                ring.prep_read(
                    self.waker.read_fd(),
                    park.wake_buf.as_mut_ptr(),
                    park.wake_buf.len() as u32,
                    OP_WAKER,
                )
            };
        }
        if !park.timeout_inflight {
            park.ts = KernelTimespec::from_millis(self.park_timeout_ms.max(1) as u64);
            // SAFETY: `ts` is only rewritten when no timeout SQE is in
            // flight, and outlives the SQE (same lifetime as `wake_buf`).
            park.timeout_inflight = unsafe { ring.prep_timeout(&park.ts, OP_TIMEOUT) };
        }
        if park.waker_armed || park.timeout_inflight {
            ring.submit_and_wait(1)?;
        }
        self.parked[me].store(false, Ordering::SeqCst);
        Ok(())
    }
}
