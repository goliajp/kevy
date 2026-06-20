//! Raw extern "C" declarations for the syscalls the engine needs (`mmap`,
//! `munmap`, `close`, `syscall`) plus the io_uring kernel ABI constants
//! everything else in the crate references.

use core::ffi::{c_int, c_long, c_void};

unsafe extern "C" {
    pub fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        off: i64,
    ) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    /// Raw syscall: io_uring has no glibc wrapper. Variadic in C.
    pub fn syscall(num: c_long, ...) -> c_long;
}

// ---- io_uring syscall numbers — identical across Linux architectures ------

pub const SYS_IO_URING_SETUP: c_long = 425;
pub const SYS_IO_URING_ENTER: c_long = 426;
pub const SYS_IO_URING_REGISTER: c_long = 427;

// ---- mmap protection / flags ----------------------------------------------

pub const PROT_READ: c_int = 0x1;
pub const PROT_WRITE: c_int = 0x2;
pub const MAP_SHARED: c_int = 0x1;
pub const MAP_PRIVATE: c_int = 0x2;
pub const MAP_ANONYMOUS: c_int = 0x20;
pub const MAP_POPULATE: c_int = 0x8000;

// ---- mmap region offsets (file-offset selectors for the three regions) ---

pub const IORING_OFF_SQ_RING: i64 = 0;
pub const IORING_OFF_CQ_RING: i64 = 0x0800_0000;
pub const IORING_OFF_SQES: i64 = 0x1000_0000;

// ---- io_uring_setup flags -------------------------------------------------

/// Run the kernel-side submission poll thread (SQPOLL). With this flag set,
/// the kernel polls the SQ from a dedicated kernel thread and does
/// `io_uring_enter` becomes unnecessary on the steady state — submissions are
/// reaped without a syscall.
pub const IORING_SETUP_SQPOLL: u32 = 1 << 1;

/// Pin the SQPOLL kernel thread to `sq_thread_cpu`. Requires `IORING_SETUP_SQPOLL`.
pub const IORING_SETUP_SQ_AFF: u32 = 1 << 2;

/// **Linux 5.19+**. Hint that all SQEs come from "cooperative task" context
/// (the user thread is itself processing CQEs). Lets the kernel skip a
/// `task_work_add`/IPI on the completion path. Free win when the same
/// thread that calls `io_uring_enter` is the one that drains CQEs.
pub const IORING_SETUP_COOP_TASKRUN: u32 = 1 << 8;

/// **Linux 6.0+**. Declare that **only one thread** ever submits to this
/// ring. Lets the kernel skip locking on the submission path. Safe for
/// kevy's per-shard rings (one shard thread owns each ring exclusively).
pub const IORING_SETUP_SINGLE_ISSUER: u32 = 1 << 12;

/// **Linux 6.1+**. Defer all completion task_work to the user thread's
/// `io_uring_enter` call instead of running it from an IPI. Pairs with
/// `SINGLE_ISSUER` and slashes the cost of completion-side bookkeeping.
/// Requires `SINGLE_ISSUER` set as well.
///
/// **Defined but not used in kevy** — see the E2 attack notes in
/// `bench/PERF-ATTACK-LOG-2026-06-20.md`. The constant is kept in the
/// ABI table for documentation + future single-threaded reactor callers.
#[allow(dead_code)]
pub const IORING_SETUP_DEFER_TASKRUN: u32 = 1 << 13;

// ---- io_uring_enter flags -------------------------------------------------

pub const IORING_ENTER_GETEVENTS: u32 = 1;

/// Wake the SQPOLL kernel thread if it was parked. Userland must check the
/// `IORING_SQ_NEED_WAKEUP` bit in the shared `sq_flags` and pass this flag
/// to `io_uring_enter` whenever it is set.
pub const IORING_ENTER_SQ_WAKEUP: u32 = 1 << 1;

// ---- shared SQ ring flag bits ---------------------------------------------

/// The SQPOLL kernel thread has parked itself (idle longer than
/// `sq_thread_idle` ms). Userland MUST call `io_uring_enter` with
/// `IORING_ENTER_SQ_WAKEUP` to re-arm it.
pub const IORING_SQ_NEED_WAKEUP: u32 = 1 << 0;

// ---- Operation opcodes (subset we use) ------------------------------------

pub const IORING_OP_NOP: u8 = 0;
pub const IORING_OP_TIMEOUT: u8 = 11;
pub const IORING_OP_ACCEPT: u8 = 13;
pub const IORING_OP_READ: u8 = 22;
pub const IORING_OP_WRITE: u8 = 23;
pub const IORING_OP_RECV: u8 = 27;

// accept4 flags set on the accepted socket (carried in the SQE's accept_flags
// field, which aliases `rw_flags`).
pub const SOCK_NONBLOCK: u32 = 0x800;
pub const SOCK_CLOEXEC: u32 = 0x8_0000;

// ---- SQE flags / ioprio bits for buffer-select + multishot recv -----------

pub const IOSQE_BUFFER_SELECT: u8 = 1 << 5; // SQE picks a buffer from a group
pub const IORING_RECV_MULTISHOT: u16 = 2; // (ioprio) re-fire one recv per arrival

// ---- io_uring_register opcodes --------------------------------------------

/// Defined for completeness — the registered files table is auto-released
/// when the ring fd closes, so explicit unregister is unused.
#[allow(dead_code)]
pub const IORING_REGISTER_FILES: c_int = 2;
#[allow(dead_code)]
pub const IORING_UNREGISTER_FILES: c_int = 3;
/// **Linux 5.13+**. Replace one slot's fd in a previously-registered files
/// table. Caller passes a `struct io_uring_files_update` describing the
/// slot index + fd; -1 in the fd field unmaps the slot.
pub const IORING_REGISTER_FILES_UPDATE: c_int = 6;
/// **Linux 5.13+**. Register an files table via the rsrc-struct API. Pair
/// with `IORING_RSRC_REGISTER_SPARSE` in the struct's flags to allocate
/// an empty table of `nr` slots without supplying initial fds.
pub const IORING_REGISTER_FILES2: c_int = 13;

pub const IORING_REGISTER_PBUF_RING: c_int = 22;
pub const IORING_UNREGISTER_PBUF_RING: c_int = 23;

/// **Linux 5.18+**. Register the ring's own fd into the user task's
/// io_uring-registered-rings table. After registration, callers pass the
/// returned index (with `IORING_ENTER_REGISTERED_RING` set) instead of
/// the raw ring fd; the kernel skips `fget`/`fput` per `io_uring_enter`
/// syscall — the largest visible kernel-side cost in kevy's perf-record
/// (5.5% / 2.7% of -c1 CPU before this attack).
pub const IORING_REGISTER_RING_FDS: c_int = 20;
#[allow(dead_code)]
pub const IORING_UNREGISTER_RING_FDS: c_int = 21;

/// **Linux 5.18+**. Tells `io_uring_enter` that its `fd` argument is an
/// index into the registered-rings table from
/// [`IORING_REGISTER_RING_FDS`], not a raw fd.
pub const IORING_ENTER_REGISTERED_RING: u32 = 1 << 4;

// ---- SQE flags for fixed-file ops ----------------------------------------

/// **Linux 5.1+**. Treat the SQE's `fd` field as an **index into the
/// registered files table** (see [`IORING_REGISTER_FILES_SPARSE`]) instead
/// of a real fd. The kernel skips the per-op `fget`/`fput` fd-table lookup
/// — the largest single non-Spectre kernel cost in kevy's hot path
/// (8 pp of -c1 CPU on the lx64 reference; see attack E1).
pub const IOSQE_FIXED_FILE: u8 = 1 << 0;

// ---- Completion `flags` bits ----------------------------------------------
// A buffer was used (id in the top 16 bits) / the multishot SQE remains armed.

pub const IORING_CQE_F_BUFFER: u32 = 1 << 0;
pub const IORING_CQE_F_MORE: u32 = 1 << 1;
pub const IORING_CQE_BUFFER_SHIFT: u32 = 16;

// ---- Provided-buffer ring layout constants --------------------------------

/// `sizeof(struct io_uring_buf)` — `{ addr:u64, len:u32, bid:u16, resv:u16 }`.
pub const IO_URING_BUF_SIZE: usize = 16;
/// Byte offset of the producer `tail` within the buf ring (it aliases
/// `bufs[0].resv`, so adding a buffer at index 0 — which writes only addr/len/bid,
/// offsets 0..14 — never clobbers it).
pub const IO_URING_BUF_TAIL_OFF: usize = 14;
