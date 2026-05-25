//! Linux `io_uring` — a **completion**-based I/O engine, the counterpart to the
//! readiness [`Poller`](crate::Poller).
//!
//! Where epoll/kqueue tell you *when* an fd is ready (then you do a `read`/`write`
//! syscall each), io_uring lets you **submit** the reads/writes/accepts themselves
//! into a shared submission queue (SQ) and later reap their results from a
//! completion queue (CQ) — batching many operations into one `io_uring_enter`
//! syscall, the lever toward the disk-I/O ceiling. Linux-only; macOS keeps using
//! the poller.
//!
//! This is hand-written against the kernel ABI — `io_uring_setup`/`io_uring_enter`
//! are raw syscalls (no glibc wrappers, no `liburing` C dependency); the SQ/CQ/SQE
//! regions are `mmap`'d and driven through the documented head/tail cursors. Pure
//! Rust at the OS boundary, consistent with the rest of [`crate`].
//!
//! # Safety
//!
//! The shared ring cursors are accessed as [`AtomicU32`] over the `mmap`'d memory
//! (the kernel is the other party): the producer publishes the SQ tail with
//! `Release` and reads the SQ head with `Acquire`; the consumer reads the CQ tail
//! with `Acquire` and publishes the CQ head with `Release`. [`IoUring`] owns its
//! ring fd and three mappings, freed on drop.

use core::ffi::{c_int, c_long, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use std::io;

mod ffi {
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
        // Raw syscall: io_uring has no glibc wrapper. Variadic in C.
        pub fn syscall(num: c_long, ...) -> c_long;
    }
}

// io_uring syscall numbers — identical across Linux architectures.
const SYS_IO_URING_SETUP: c_long = 425;
const SYS_IO_URING_ENTER: c_long = 426;

// mmap protection / flags.
const PROT_READ: c_int = 0x1;
const PROT_WRITE: c_int = 0x2;
const MAP_SHARED: c_int = 0x1;
const MAP_POPULATE: c_int = 0x8000;

// `mmap` region offsets passed as the file offset to select SQ ring / CQ ring / SQEs.
const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x0800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;

// `io_uring_enter` flags.
const IORING_ENTER_GETEVENTS: u32 = 1;

// Operation opcodes (subset we use).
const IORING_OP_NOP: u8 = 0;
const IORING_OP_ACCEPT: u8 = 13;
const IORING_OP_READ: u8 = 22;
const IORING_OP_WRITE: u8 = 23;

// accept4 flags set on the accepted socket (carried in the SQE's accept_flags
// field, which aliases `rw_flags`).
const SOCK_NONBLOCK: u32 = 0x800;
const SOCK_CLOEXEC: u32 = 0x8_0000;

#[repr(C)]
#[derive(Default)]
struct IoSqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    resv2: u64,
}

#[repr(C)]
#[derive(Default)]
struct IoCqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    resv2: u64,
}

#[repr(C)]
#[derive(Default)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: IoSqringOffsets,
    cq_off: IoCqringOffsets,
}

/// `struct io_uring_sqe` — the 64-byte submission entry.
#[repr(C)]
struct IoUringSqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    rw_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    addr3: u64,
    __pad2: u64,
}

impl IoUringSqe {
    /// A zeroed SQE with the common fields set. Op-specific fields (e.g.
    /// `rw_flags` for accept flags) are tweaked by the caller afterward.
    fn new(opcode: u8, fd: i32, addr: u64, len: u32, user_data: u64) -> IoUringSqe {
        IoUringSqe {
            opcode,
            flags: 0,
            ioprio: 0,
            fd,
            off: 0,
            addr,
            len,
            rw_flags: 0,
            user_data,
            buf_index: 0,
            personality: 0,
            splice_fd_in: 0,
            addr3: 0,
            __pad2: 0,
        }
    }
}

/// One reaped completion (`struct io_uring_cqe`): the `user_data` you tagged the
/// submission with, and `res` (bytes transferred / accepted fd when ≥ 0, else
/// `-errno`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Completion {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

/// A Linux io_uring instance: one submission ring + one completion ring.
pub struct IoUring {
    ring_fd: c_int,
    sq_mmap: *mut c_void,
    sq_mmap_len: usize,
    cq_mmap: *mut c_void,
    cq_mmap_len: usize,
    sqes: *mut IoUringSqe,
    sqes_len: usize,
    sq_entries: u32,
    sq_mask: u32,
    /// Local producer cursor; published to the kernel on `submit`.
    sq_tail: u32,
    sq_khead: *const AtomicU32,
    sq_ktail: *const AtomicU32,
    sq_array: *mut u32,
    cq_mask: u32,
    cq_khead: *const AtomicU32,
    cq_ktail: *const AtomicU32,
    cqes: *const Completion,
}

// SAFETY: `IoUring` owns its fd and mappings exclusively; moving the whole engine
// to another thread (one per shard) is sound. It is not `Sync` (single owner).
unsafe impl Send for IoUring {}

impl IoUring {
    /// Create a ring sized for at least `entries` in-flight submissions.
    pub fn new(entries: u32) -> io::Result<IoUring> {
        let mut p = IoUringParams::default();
        let fd = unsafe {
            ffi::syscall(SYS_IO_URING_SETUP, entries as c_long, &mut p as *mut IoUringParams)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let ring_fd = fd as c_int;

        let sq_len = (p.sq_off.array as usize) + (p.sq_entries as usize) * 4;
        let cq_len =
            (p.cq_off.cqes as usize) + (p.cq_entries as usize) * core::mem::size_of::<Completion>();
        let sqes_len = (p.sq_entries as usize) * core::mem::size_of::<IoUringSqe>();

        let map = |len: usize, off: i64| -> *mut c_void {
            unsafe {
                ffi::mmap(
                    ptr::null_mut(),
                    len,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED | MAP_POPULATE,
                    ring_fd,
                    off,
                )
            }
        };
        let failed = |m: *mut c_void| m as isize == -1;

        let sq_mmap = map(sq_len, IORING_OFF_SQ_RING);
        if failed(sq_mmap) {
            let e = io::Error::last_os_error();
            unsafe { ffi::close(ring_fd) };
            return Err(e);
        }
        let cq_mmap = map(cq_len, IORING_OFF_CQ_RING);
        if failed(cq_mmap) {
            let e = io::Error::last_os_error();
            unsafe {
                ffi::munmap(sq_mmap, sq_len);
                ffi::close(ring_fd);
            }
            return Err(e);
        }
        let sqes_map = map(sqes_len, IORING_OFF_SQES);
        if failed(sqes_map) {
            let e = io::Error::last_os_error();
            unsafe {
                ffi::munmap(cq_mmap, cq_len);
                ffi::munmap(sq_mmap, sq_len);
                ffi::close(ring_fd);
            }
            return Err(e);
        }

        let sq_base = sq_mmap as usize;
        let cq_base = cq_mmap as usize;
        let at = |base: usize, off: u32| (base + off as usize) as *const AtomicU32;
        let sq_khead = at(sq_base, p.sq_off.head);
        let sq_ktail = at(sq_base, p.sq_off.tail);
        let sq_array = (sq_base + p.sq_off.array as usize) as *mut u32;
        let sq_mask = unsafe { *((sq_base + p.sq_off.ring_mask as usize) as *const u32) };
        let cq_khead = at(cq_base, p.cq_off.head);
        let cq_ktail = at(cq_base, p.cq_off.tail);
        let cqes = (cq_base + p.cq_off.cqes as usize) as *const Completion;
        let cq_mask = unsafe { *((cq_base + p.cq_off.ring_mask as usize) as *const u32) };
        let sq_tail = unsafe { (*sq_ktail).load(Ordering::Acquire) };

        Ok(IoUring {
            ring_fd,
            sq_mmap,
            sq_mmap_len: sq_len,
            cq_mmap,
            cq_mmap_len: cq_len,
            sqes: sqes_map as *mut IoUringSqe,
            sqes_len,
            sq_entries: p.sq_entries,
            sq_mask,
            sq_tail,
            sq_khead,
            sq_ktail,
            sq_array,
            cq_mask,
            cq_khead,
            cq_ktail,
            cqes,
        })
    }

    /// Reserve the next SQ slot (advancing the producer cursor + array map);
    /// returns its SQE index, or `None` if the submission queue is full.
    fn reserve(&mut self) -> Option<usize> {
        let khead = unsafe { (*self.sq_khead).load(Ordering::Acquire) };
        if self.sq_tail.wrapping_sub(khead) >= self.sq_entries {
            return None; // SQ full
        }
        let idx = (self.sq_tail & self.sq_mask) as usize;
        // The SQ `array` maps a ring slot to an SQE index (here 1:1).
        unsafe { *self.sq_array.add(idx) = idx as u32 };
        self.sq_tail = self.sq_tail.wrapping_add(1);
        Some(idx)
    }

    /// Queue a `read(fd)` of `len` bytes into `buf`, tagged with `user_data`.
    /// Returns `false` if the SQ is full.
    ///
    /// # Safety
    /// `buf` must point to `len` writable bytes and stay valid until the matching
    /// completion is reaped.
    pub unsafe fn prep_read(&mut self, fd: i32, buf: *mut u8, len: u32, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            ptr::write(self.sqes.add(idx), IoUringSqe::new(IORING_OP_READ, fd, buf as u64, len, user_data));
        }
        true
    }

    /// Queue a `write(fd)` of `len` bytes from `buf`, tagged with `user_data`.
    /// Returns `false` if the SQ is full.
    ///
    /// # Safety
    /// `buf` must point to `len` readable bytes and stay valid until the matching
    /// completion is reaped.
    pub unsafe fn prep_write(&mut self, fd: i32, buf: *const u8, len: u32, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            ptr::write(self.sqes.add(idx), IoUringSqe::new(IORING_OP_WRITE, fd, buf as u64, len, user_data));
        }
        true
    }

    /// Queue an `accept` on `listen_fd`; the accepted fd arrives as the
    /// completion's `res` (already `O_NONBLOCK | O_CLOEXEC`). Returns `false` if
    /// the SQ is full.
    pub fn prep_accept(&mut self, listen_fd: i32, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            let sqe = self.sqes.add(idx);
            ptr::write(sqe, IoUringSqe::new(IORING_OP_ACCEPT, listen_fd, 0, 0, user_data));
            (*sqe).rw_flags = SOCK_NONBLOCK | SOCK_CLOEXEC;
        }
        true
    }

    /// Queue a no-op tagged with `user_data` (used to prove the round-trip).
    /// Returns `false` if the SQ is full.
    pub fn prep_nop(&mut self, user_data: u64) -> bool {
        let Some(idx) = self.reserve() else {
            return false;
        };
        // SAFETY: `idx` is a freshly reserved, in-bounds SQE slot we own alone.
        unsafe {
            ptr::write(self.sqes.add(idx), IoUringSqe::new(IORING_OP_NOP, -1, 0, 0, user_data));
        }
        true
    }

    /// Publish queued submissions and enter the kernel, optionally waiting for
    /// `wait_nr` completions. Returns the number of SQEs consumed.
    pub fn submit_and_wait(&mut self, wait_nr: u32) -> io::Result<u32> {
        let prev = unsafe { (*self.sq_ktail).load(Ordering::Relaxed) };
        let to_submit = self.sq_tail.wrapping_sub(prev);
        unsafe { (*self.sq_ktail).store(self.sq_tail, Ordering::Release) };
        let flags = if wait_nr > 0 { IORING_ENTER_GETEVENTS } else { 0 };
        let ret = unsafe {
            ffi::syscall(
                SYS_IO_URING_ENTER,
                self.ring_fd as c_long,
                to_submit as c_long,
                wait_nr as c_long,
                flags as c_long,
                ptr::null::<c_void>(),
                0usize,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ret as u32)
    }

    /// Reap every available completion, calling `f` for each; returns the count.
    pub fn for_each_completion<F: FnMut(Completion)>(&mut self, mut f: F) -> u32 {
        let mut head = unsafe { (*self.cq_khead).load(Ordering::Relaxed) };
        let tail = unsafe { (*self.cq_ktail).load(Ordering::Acquire) };
        let mut n = 0;
        while head != tail {
            let idx = (head & self.cq_mask) as usize;
            let cqe = unsafe { *self.cqes.add(idx) };
            f(cqe);
            head = head.wrapping_add(1);
            n += 1;
        }
        unsafe { (*self.cq_khead).store(head, Ordering::Release) };
        n
    }
}

impl Drop for IoUring {
    fn drop(&mut self) {
        unsafe {
            ffi::munmap(self.sqes as *mut c_void, self.sqes_len);
            ffi::munmap(self.cq_mmap, self.cq_mmap_len);
            ffi::munmap(self.sq_mmap, self.sq_mmap_len);
            ffi::close(self.ring_fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    // io_uring may be unavailable under a restricted seccomp profile (Docker's
    // default blocks io_uring_setup → EPERM/ENOSYS). Run with
    // `--security-opt seccomp=unconfined` so these actually exercise the engine;
    // they skip (rather than fail) where the syscall is denied.
    fn ring_or_skip(entries: u32) -> Option<IoUring> {
        match IoUring::new(entries) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("SKIP: io_uring unavailable ({e})");
                None
            }
        }
    }

    #[test]
    fn nop_round_trips() {
        let Some(mut ring) = ring_or_skip(8) else {
            return;
        };
        assert!(ring.prep_nop(0x1234));
        assert_eq!(ring.submit_and_wait(1).unwrap(), 1);
        let mut got = None;
        let n = ring.for_each_completion(|c| got = Some(c));
        assert_eq!(n, 1);
        let c = got.expect("one completion");
        assert_eq!(c.user_data, 0x1234);
        assert_eq!(c.res, 0); // NOP succeeds with res 0
    }

    #[test]
    fn reads_a_file() {
        let Some(mut ring) = ring_or_skip(8) else {
            return;
        };
        let path = std::env::temp_dir().join(format!("kevy-uring-{}", std::process::id()));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"hello io_uring").unwrap();
            f.sync_all().unwrap();
        }
        let file = std::fs::File::open(&path).unwrap();
        let mut buf = [0u8; 64];
        unsafe {
            assert!(ring.prep_read(file.as_raw_fd(), buf.as_mut_ptr(), buf.len() as u32, 0xABCD));
        }
        assert_eq!(ring.submit_and_wait(1).unwrap(), 1);
        let mut got = None;
        ring.for_each_completion(|c| got = Some(c));
        let c = got.expect("one completion");
        assert_eq!(c.user_data, 0xABCD);
        assert_eq!(c.res, 14, "should read 14 bytes");
        assert_eq!(&buf[..14], b"hello io_uring");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn batched_nops() {
        // Submit a full batch, reap them all — exercises ring wrap + counts.
        let Some(mut ring) = ring_or_skip(8) else {
            return;
        };
        for i in 0..8u64 {
            assert!(ring.prep_nop(i));
        }
        assert!(!ring.prep_nop(99), "9th submission should report SQ full");
        assert_eq!(ring.submit_and_wait(8).unwrap(), 8);
        let mut seen = 0u64;
        let n = ring.for_each_completion(|c| seen |= 1 << c.user_data);
        assert_eq!(n, 8);
        assert_eq!(seen, 0xFF, "all 8 user_data tags present");
    }

    #[test]
    fn accepts_a_connection() {
        // io_uring ACCEPT: a pending connection on the listener is accepted and
        // its fd arrives as the completion's `res` (≥ 0).
        let Some(mut ring) = ring_or_skip(8) else {
            return;
        };
        let listener = crate::tcp_listen([127, 0, 0, 1], 0, 128).unwrap();
        let port = listener.local_port().unwrap();
        // Connect first so the accept can complete immediately from the backlog.
        let _client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();

        assert!(ring.prep_accept(listener.raw(), 0xACCE));
        assert_eq!(ring.submit_and_wait(1).unwrap(), 1);
        let mut got = None;
        ring.for_each_completion(|c| got = Some(c));
        let c = got.expect("accept completion");
        assert_eq!(c.user_data, 0xACCE);
        assert!(c.res >= 0, "accepted fd should be >= 0, got {}", c.res);
        // SAFETY: `c.res` is the freshly accepted fd; wrap so drop closes it.
        let _ = unsafe { OwnedFd::from_raw_fd(c.res) };
    }

    #[test]
    fn echo_round_trip_via_io_uring() {
        // Drive a full accept → read → write echo entirely through io_uring —
        // the exact completion loop the Phase-2 reactor will run. A client thread
        // connects, sends, and verifies the echo.
        const ACCEPT: u64 = 1;
        const READ: u64 = 2;
        const WRITE: u64 = 3;

        let Some(mut ring) = ring_or_skip(16) else {
            return;
        };
        let listener = crate::tcp_listen([127, 0, 0, 1], 0, 128).unwrap();
        let port = listener.local_port().unwrap();

        let client = std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(b"ping").unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"ping", "client should receive the echo");
        });

        // accept (blocks in the kernel until the client connects)
        assert!(ring.prep_accept(listener.raw(), ACCEPT));
        ring.submit_and_wait(1).unwrap();
        let mut conn_fd = -1;
        ring.for_each_completion(|c| {
            if c.user_data == ACCEPT {
                conn_fd = c.res;
            }
        });
        assert!(conn_fd >= 0, "accept failed: {conn_fd}");

        // read the request
        let mut rbuf = [0u8; 64];
        unsafe { assert!(ring.prep_read(conn_fd, rbuf.as_mut_ptr(), rbuf.len() as u32, READ)) };
        ring.submit_and_wait(1).unwrap();
        let mut nread = 0;
        ring.for_each_completion(|c| {
            if c.user_data == READ {
                nread = c.res;
            }
        });
        assert_eq!(nread, 4, "should read 4 bytes");
        assert_eq!(&rbuf[..4], b"ping");

        // write the echo back
        unsafe { assert!(ring.prep_write(conn_fd, rbuf.as_ptr(), 4, WRITE)) };
        ring.submit_and_wait(1).unwrap();
        let mut nwrote = 0;
        ring.for_each_completion(|c| {
            if c.user_data == WRITE {
                nwrote = c.res;
            }
        });
        assert_eq!(nwrote, 4, "should write 4 bytes");

        client.join().unwrap();
        // SAFETY: `conn_fd` is the accepted fd; wrap so drop closes it.
        let _ = unsafe { OwnedFd::from_raw_fd(conn_fd) };
    }
}
