//! kevy-uring — pure-Rust `io_uring` bindings against the Linux kernel ABI.
//!
//! A **completion**-based I/O engine. Where epoll/kqueue tell you *when* an
//! fd is ready (then you do a `read`/`write` syscall each), io_uring lets
//! you **submit** the reads/writes/accepts themselves into a shared
//! submission queue (SQ) and later reap their results from a completion
//! queue (CQ) — batching many operations into one `io_uring_enter` syscall,
//! the lever toward the disk-I/O ceiling. **Linux-only**: on every other
//! target this crate is an empty module that any caller can `cfg`-gate.
//!
//! Hand-written against the kernel ABI — `io_uring_setup`/`io_uring_enter`/
//! `io_uring_register` are raw syscalls (no glibc wrappers, no `liburing`
//! C dependency); the SQ/CQ/SQE regions are `mmap`'d and driven through
//! the documented head/tail cursors. **No `libc` crate, no third-party
//! dependency.**
//!
//! Carved out of [`kevy-sys`](https://crates.io/crates/kevy-sys) so the
//! engine can be reused independently of kevy's network internals. Part of
//! the [kevy](https://crates.io/crates/kevy) key–value server.
//!
//! # Safety
//!
//! The shared ring cursors are accessed as `AtomicU32` over the `mmap`'d
//! memory (the kernel is the other party): the producer publishes the SQ
//! tail with `Release` and reads the SQ head with `Acquire`; the consumer
//! reads the CQ tail with `Acquire` and publishes the CQ head with
//! `Release`. `IoUring` owns its ring fd and three mappings, freed on
//! drop.
//!
//! # Example
//!
//! ```no_run
//! use kevy_uring::IoUring;
//!
//! let mut ring = IoUring::new(8)?;
//! assert!(ring.prep_nop(7), "the queue has room");
//! ring.submit_and_wait(1)?;
//! let mut tags = Vec::new();
//! ring.for_each_completion(|c| tags.push(c.user_data));
//! assert_eq!(tags, vec![7]);
//! # Ok::<(), std::io::Error>(())
//! ```

#![cfg(target_os = "linux")]
#![warn(missing_docs)]

mod completion;
mod enter_policy;
mod ffi;
mod file_batch;
mod layout;
mod pbr;
mod prep;
mod register;
mod ring;
mod setup;

#[cfg(test)]
mod ring_tests;

pub use completion::Completion;
pub use ffi::Iovec;
pub use file_batch::FileRead;
pub use layout::KernelTimespec;
pub use pbr::ProvidedBufRing;
pub use ring::IoUring;

// Send and Sync are part of the public contract: a change that loses
// either fails to compile here rather than in a caller.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    const fn send<T: Send>() {}
    send_sync::<Completion>();
    send_sync::<FileRead>();
    send_sync::<KernelTimespec>();
    send::<IoUring>();
    send::<ProvidedBufRing>();
};
