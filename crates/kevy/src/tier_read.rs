//! Cold-batch read issuance (capacity arc T6, RFC §1 D2/D4): which
//! reader a hydration page's cold rows go through.
//!
//! - **Linux**: a small per-shard-thread SECONDARY io_uring ring
//!   dedicated to vlog file reads — up to [`uring::RING_ENTRIES`] READ
//!   SQEs per `io_uring_enter`, submitted and WAITED synchronously
//!   inside the op (one enter for N reads; the kernel runs them
//!   concurrently). The socket ring and the reactor CQE loop are never
//!   touched; fully-async cold reads are post-v4 by RFC §7. Created
//!   lazily on the first cold batch; honors `KEVY_IO_URING=0/off/no/
//!   false` (the reactor's own kill-switch) and degrades loudly to the
//!   sync loop when the host can't build a ring (seccomp, pre-5.19) —
//!   the same auto posture as `kevy_rt`'s reactor choice.
//! - **Everything else** (macOS kqueue, forced-epoll fallback): the
//!   ordered positional-read loop [`kevy_store::SyncColdRead`].

use kevy_store::ColdBatchReader;
#[cfg(not(target_os = "linux"))]
use kevy_store::SyncColdRead;

/// Run `f` with this thread's cold-batch reader.
pub(crate) fn with_cold_reader<R>(f: impl FnOnce(&mut dyn ColdBatchReader) -> R) -> R {
    #[cfg(target_os = "linux")]
    {
        uring::with_reader(f)
    }
    #[cfg(not(target_os = "linux"))]
    {
        f(&mut SyncColdRead)
    }
}

#[cfg(target_os = "linux")]
mod uring {
    use std::cell::RefCell;
    use std::io;

    use kevy_store::{ColdBatchReader, ColdRead, SyncColdRead};
    use kevy_uring::IoUring;

    /// Secondary-ring depth: one hydration page is ≤ a few hundred
    /// rows; 64 in-flight preads saturate an NVMe queue while keeping
    /// the ring's footprint trivial. Batches larger than the SQ chunk
    /// across multiple submits.
    const RING_ENTRIES: u32 = 64;

    enum Slot {
        Untried,
        Unavailable,
        Ready(IoUring),
    }

    thread_local! {
        /// One ring per shard thread (ops run on their shard's thread),
        /// created lazily on the first cold batch.
        static RING: RefCell<Slot> = const { RefCell::new(Slot::Untried) };
    }

    /// The reactor's own kill-switch disables the secondary ring too:
    /// an operator forcing epoll (`KEVY_IO_URING=0`) gets a server
    /// with zero io_uring syscalls.
    fn uring_off() -> bool {
        matches!(
            std::env::var("KEVY_IO_URING").ok().as_deref(),
            Some("0") | Some("off") | Some("no") | Some("false")
        )
    }

    pub(super) fn with_reader<R>(f: impl FnOnce(&mut dyn ColdBatchReader) -> R) -> R {
        RING.with(|slot| {
            let mut slot = slot.borrow_mut();
            if matches!(*slot, Slot::Untried) {
                *slot = if uring_off() {
                    Slot::Unavailable
                } else {
                    match IoUring::new(RING_ENTRIES) {
                        Ok(ring) => Slot::Ready(ring),
                        Err(e) => {
                            eprintln!(
                                "kevy: tier cold-batch ring setup failed ({e}); \
                                 using the ordered pread loop on this shard"
                            );
                            Slot::Unavailable
                        }
                    }
                };
            }
            match &mut *slot {
                Slot::Ready(ring) => f(&mut UringColdRead { ring }),
                _ => f(&mut SyncColdRead),
            }
        })
    }

    /// [`ColdBatchReader`] over the secondary ring, via the safe
    /// [`IoUring::read_file_batch`] primitive (buffers owned inside
    /// kevy-uring — this crate stays `forbid(unsafe_code)`-clean). The
    /// pinned `Arc<VlogFile>`s in `reads` keep every fd open across
    /// the wait. A short or failed pread of a record this process
    /// wrote is a process bug (the kevy-vlog doctrine) and surfaces as
    /// an error.
    struct UringColdRead<'a> {
        ring: &'a mut IoUring,
    }

    impl ColdBatchReader for UringColdRead<'_> {
        fn read_batch(&mut self, reads: &[ColdRead]) -> io::Result<(Vec<Vec<u8>>, u64)> {
            let plan: Vec<kevy_uring::FileRead> = reads
                .iter()
                .map(|r| kevy_uring::FileRead {
                    fd: r.file.raw_fd(),
                    offset: r.vref.offset,
                    len: r.vref.disk_len() as u32,
                })
                .collect();
            self.ring.read_file_batch(&plan)
        }
    }
}
