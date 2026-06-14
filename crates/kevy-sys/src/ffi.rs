//! Hand-rolled libc bindings — the only `unsafe extern "C"` boundary in
//! the kevy server. Lives in its own file to keep [`crate::lib`] under
//! the 500-LOC house rule. Stays `pub(crate)` — every consumer is a
//! sibling module of this crate.

use core::ffi::{c_int, c_void};

// socklen_t is u32 on both Linux and macOS.
unsafe extern "C" {
    pub fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    pub fn setsockopt(
        fd: c_int,
        level: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: u32,
    ) -> c_int;
    pub fn bind(fd: c_int, addr: *const c_void, addrlen: u32) -> c_int;
    pub fn listen(fd: c_int, backlog: c_int) -> c_int;
    pub fn accept(fd: c_int, addr: *mut c_void, addrlen: *mut u32) -> c_int;
    pub fn getsockname(fd: c_int, addr: *mut c_void, addrlen: *mut u32) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    pub fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    pub fn close(fd: c_int) -> c_int;
    // Variadic in C; we only ever pass a single int arg (F_GETFL/F_SETFL).
    pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    pub fn pipe(fds: *mut c_int) -> c_int;
}

/// `struct timespec` — used by kqueue's `kevent` timeout (macOS only).
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[repr(C)]
pub struct Timespec {
    pub tv_sec: isize,
    pub tv_nsec: isize,
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[repr(C)]
pub struct Kevent {
    pub ident: usize,
    pub filter: i16,
    pub flags: u16,
    pub fflags: u32,
    pub data: isize,
    pub udata: usize, // really `void*`; we never use it, keep it integral (Send)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
unsafe extern "C" {
    pub fn kqueue() -> c_int;
    pub fn kevent(
        kq: c_int,
        changelist: *const Kevent,
        nchanges: c_int,
        eventlist: *mut Kevent,
        nevents: c_int,
        timeout: *const Timespec,
    ) -> c_int;
}

// `struct epoll_event` is `__attribute__((packed))` only on x86_64; on every
// other arch it is naturally aligned (8-byte `data` after 4-byte `events`,
// with 4 bytes of padding). Match the kernel ABI exactly.
#[cfg(target_os = "linux")]
#[repr(C)]
#[cfg_attr(target_arch = "x86_64", repr(packed))]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
    pub fn epoll_wait(
        epfd: c_int,
        events: *mut EpollEvent,
        maxevents: c_int,
        timeout: c_int,
    ) -> c_int;
    // `cpu_set_t` is an opaque bitmask; we pass our own `[u64; N]` and its
    // byte length, exactly as glibc's `CPU_SET` macros lay it out.
    pub fn sched_getaffinity(pid: c_int, cpusetsize: usize, mask: *mut u64) -> c_int;
    pub fn sched_setaffinity(pid: c_int, cpusetsize: usize, mask: *const u64) -> c_int;
}
