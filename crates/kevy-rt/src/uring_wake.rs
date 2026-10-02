//! Waking a parked peer from the io_uring reactor by messaging its ring:
//! the wake rides this shard's next enter, and the woken shard reads a
//! completion instead of draining an eventfd. Same `impl Shard`, called
//! from `run_uring`; the epoll reactor keeps the eventfd wake.

use std::sync::atomic::Ordering;

use kevy_uring::IoUring;

use crate::Commands;
use crate::park_fence;
use crate::shard::Shard;
use crate::uring_ops::{CONN_MASK, OP_MSG_FAIL, OP_MSG_WAKE};

impl<C: Commands> Shard<C> {
    /// Offer this shard's ring to peers as a wake target, if the kernel
    /// delivers ring messages. Runs on the fresh ring, before anything is
    /// queued on it.
    pub(crate) fn uring_publish_ring(&self, ring: &mut IoUring) {
        if ring.msg_ring_works() {
            self.ring_fds[self.id].store(ring.raw_fd(), Ordering::Release);
        }
    }

    /// Withdraw the ring before it closes; a peer that still messages the
    /// old fd gets a failed send and falls back to the eventfd.
    pub(crate) fn uring_withdraw_ring(&self) {
        self.ring_fds[self.id].store(-1, Ordering::Release);
    }

    /// [`Shard::flush_wakes`] for the io_uring reactor: a parked peer with
    /// a published ring is messaged, any other gets its eventfd written.
    #[inline]
    pub(crate) fn uring_flush_wakes(&mut self, ring: &mut IoUring) {
        if self.pending_wakes == 0 {
            return;
        }
        self.uring_flush_wakes_slow(ring);
    }

    #[inline(never)]
    fn uring_flush_wakes_slow(&mut self, ring: &mut IoUring) {
        // the fence and its pairing are the ones `flush_wakes_slow` documents
        park_fence::fence_before_wake_scan();
        let mut mask = self.pending_wakes;
        self.pending_wakes = 0;
        while mask != 0 {
            let i = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            if !park_fence::peer_is_parked(&self.parked[i]) {
                continue;
            }
            let fd = self.ring_fds[i].load(Ordering::Acquire);
            if fd < 0 || !ring.prep_msg_ring(fd, OP_MSG_WAKE, OP_MSG_FAIL | i as u64) {
                let _ = self.wakers[i].wake();
            }
        }
    }

    /// A message to peer `user_data & CONN_MASK` did not go through: wake
    /// it the other way.
    #[cold]
    pub(crate) fn uring_msg_failed(&self, user_data: u64) {
        let _ = self.wakers[(user_data & CONN_MASK) as usize].wake();
    }
}
