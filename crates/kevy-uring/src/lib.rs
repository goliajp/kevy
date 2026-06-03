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

#![cfg(target_os = "linux")]

mod completion;
mod ffi;
mod layout;
mod pbr;
mod ring;

#[cfg(test)]
mod ring_tests;

pub use completion::Completion;
pub use pbr::ProvidedBufRing;
pub use ring::IoUring;
